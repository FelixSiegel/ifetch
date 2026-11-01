mod benchmarks;
mod mock_upstream;
mod monitor;
mod tests;

use anyhow::{Context, Result, bail};
use benchmarks::{BenchmarkResult, run_all_benchmarks};
use clap::Parser;
use console::style;
use mock_upstream::MockUpstream;
use monitor::{ProcessMonitor, ResourceStats};
use reqwest::blocking::Client;
use std::{
    fs,
    path::{Path, PathBuf},
    process::{Child, Command, Stdio},
    sync::atomic::Ordering,
    thread,
    time::{Duration, Instant},
};
use tests::{TestCaseResult, run_all_tests};

#[derive(Parser, Debug)]
#[command(
    name = "ifetch-testbench",
    about = "Deterministic integration test & benchmark suite for iFetch server mode"
)]
struct Args {
    /// Path to the compiled `ifetch` binary (auto-detected if omitted)
    #[arg(long)]
    bin: Option<PathBuf>,

    /// Number of download threads for `ifetch --server`
    #[arg(short = 't', long, default_value = "4")]
    threads: usize,

    /// Run only deterministic functional tests (skip benchmarks)
    #[arg(long)]
    tests_only: bool,

    /// Run only benchmarks
    #[arg(long)]
    bench_only: bool,
}

/// RAII guard that ensures the spawned `ifetch --server` process and temporary workspace
/// are always cleaned up when the suite finishes or unwinds.
struct ServerProcessGuard {
    child: Child,
    workspace_dir: PathBuf,
}

impl Drop for ServerProcessGuard {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
        let _ = fs::remove_dir_all(&self.workspace_dir);
    }
}

fn main() {
    if let Err(e) = run() {
        eprintln!("\n{} {:#}", style("[FATAL]").red().bold(), e);
        std::process::exit(1);
    }
}

fn run() -> Result<()> {
    let args = Args::parse();
    let ifetch_bin = resolve_ifetch_binary(args.bin)?;

    println!(
        "\n{}",
        style("================================================================================")
            .cyan()
            .bold()
    );
    println!(
        "{}  {}",
        style("iFetch Server Deterministic Test & Benchmark Suite")
            .cyan()
            .bold(),
        style(format!("(bin: {})", ifetch_bin.display())).dim()
    );
    println!(
        "{}\n",
        style("================================================================================")
            .cyan()
            .bold()
    );

    // 1. Start deterministic MockUpstream (MangaKatana + Image CDN + Discord Webhook receiver)
    let upstream = MockUpstream::start()?;
    println!(
        "  {} Mock Upstream Server listening on {}",
        style("•").green(),
        style(&upstream.base_url).underlined()
    );

    // 2. Prepare isolated workspace directories & reserve an ephemeral port for `ifetch --server`
    let workspace_dir =
        std::env::temp_dir().join(format!("ifetch-testbench-{}", std::process::id()));
    if workspace_dir.exists() {
        let _ = fs::remove_dir_all(&workspace_dir);
    }
    let output_dir = workspace_dir.join("downloads");
    let config_dir = workspace_dir.join("config");
    fs::create_dir_all(&output_dir)?;
    fs::create_dir_all(&config_dir)?;

    let webhook_url = format!("{}/webhook", upstream.base_url);
    let server_log = workspace_dir.join("server_stderr.log");
    let stderr_file = fs::File::create(&server_log)?;

    // 3. Spawn `ifetch --server` with `--port 0` for race-free ephemeral port binding
    let child = Command::new(&ifetch_bin)
        .arg("--server")
        .arg("--port")
        .arg("0")
        .arg("--output")
        .arg(&output_dir)
        .arg("--config")
        .arg(&config_dir)
        .arg("--threads")
        .arg(args.threads.to_string())
        .env("IFETCH_BASE_URL", &upstream.base_url)
        .env("DISCORD_WEBHOOK_URL", &webhook_url)
        .env("IFETCH_CRON_HOURS", "12")
        .env("RUST_LOG", "info")
        .stdout(Stdio::null())
        .stderr(Stdio::from(stderr_file))
        .spawn()
        .with_context(|| format!("Failed to spawn {}", ifetch_bin.display()))?;

    let server_pid = child.id();
    let mut guard = ServerProcessGuard {
        child,
        workspace_dir: workspace_dir.clone(),
    };

    let client = Client::builder()
        .no_proxy()
        .timeout(Duration::from_secs(10))
        .build()?;

    let server_url = wait_until_server_ready(&client, &mut guard.child, &server_log)?;
    println!(
        "  {} iFetch Server (PID {}) ready on {} (threads: {})\n",
        style("•").green(),
        style(server_pid).yellow().bold(),
        style(&server_url).underlined(),
        args.threads
    );

    let suite_start = Instant::now();
    let monitor = ProcessMonitor::start(server_pid, Duration::from_millis(20));

    // Measure Idle Baseline right after server startup
    let idle_marker = monitor.mark_phase_start();
    thread::sleep(Duration::from_millis(200));
    let idle_stats = monitor.record_phase("0. Idle Server Baseline", idle_marker);

    // 4. Execute Deterministic Functional Tests
    let test_results = if !args.bench_only {
        run_all_tests(
            &client,
            &server_url,
            &upstream,
            &output_dir,
            &config_dir,
            &ifetch_bin,
            &monitor,
        )
    } else {
        // Seed cyber-blade.101 metadata & chapters if running bench_only
        let _ = client
            .get(format!("{}/api/manga/cyber-blade.101/chapters", server_url))
            .send();
        let deadline = Instant::now() + Duration::from_secs(10);
        while Instant::now() < deadline {
            if let Ok(r) = client
                .get(format!("{}/api/manga/cyber-blade.101/chapters", server_url))
                .send()
                && r.status().as_u16() == 200
            {
                break;
            }
            thread::sleep(Duration::from_millis(50));
        }
        Vec::new()
    };

    // 5. Execute Server Performance & Cache Benchmarks
    let bench_results = if !args.tests_only {
        run_all_benchmarks(&server_url, &output_dir, &monitor)?
    } else {
        Vec::new()
    };

    let total_duration = suite_start.elapsed();
    let (overall_stats, phase_stats) = monitor.stop(total_duration);

    // 6. Print Reports
    if !test_results.is_empty() {
        print_test_report(&test_results);
    }
    if !bench_results.is_empty() {
        print_benchmark_report(&bench_results);
    }
    print_resource_report(&idle_stats, &overall_stats, &phase_stats, &upstream);

    upstream.shutdown();

    let failed_tests = test_results.iter().filter(|t| !t.passed).count();
    let failed_bench_reqs: usize = bench_results.iter().map(|b| b.failed_requests).sum();

    if failed_tests > 0 || failed_bench_reqs > 0 {
        bail!(
            "Suite completed with {} failed test(s) and {} failed benchmark request(s)",
            failed_tests,
            failed_bench_reqs
        );
    }

    println!(
        "\n{} All deterministic checks and benchmarks passed in {:.2}s!\n",
        style("[SUCCESS]").green().bold(),
        total_duration.as_secs_f64()
    );
    Ok(())
}

fn print_test_report(results: &[TestCaseResult]) {
    println!(
        "{}",
        style("--------------------------------------------------------------------------------")
            .dim()
    );
    println!(
        "{}",
        style(" 1. DETERMINISTIC FUNCTIONAL & ROUTE TESTS").bold()
    );
    println!(
        "{}",
        style("--------------------------------------------------------------------------------")
            .dim()
    );

    for r in results {
        let badge = if r.passed {
            style(" PASS ").on_green().black().bold()
        } else {
            style(" FAIL ").on_red().white().bold()
        };
        println!(
            " {} {:<22} {} {}",
            badge,
            style(format!("[{}]", r.category)).cyan(),
            style(r.name).bold(),
            style(format!("({:.1}ms)", r.duration.as_secs_f64() * 1000.0)).dim()
        );
        println!("        └─ {}", style(&r.details).dim());
    }

    let passed = results.iter().filter(|r| r.passed).count();
    println!(
        "\n  Summary: {}/{} functional test cases passed.\n",
        style(passed).green().bold(),
        results.len()
    );
}

fn print_benchmark_report(results: &[BenchmarkResult]) {
    println!(
        "{}",
        style("--------------------------------------------------------------------------------")
            .dim()
    );
    println!(
        "{}",
        style(" 2. SERVER THROUGHPUT & LATENCY BENCHMARKS").bold()
    );
    println!(
        "{}",
        style("--------------------------------------------------------------------------------")
            .dim()
    );
    println!(
        " {:<54} {:>5} {:>9} {:>9} {:>8} {:>8} {:>8}",
        style("Workload").underlined(),
        style("Thr").underlined(),
        style("Req/s").underlined(),
        style("MiB/s").underlined(),
        style("p50").underlined(),
        style("p95").underlined(),
        style("p99").underlined(),
    );

    for b in results {
        println!(
            " {:<54} {:>5} {:>9.1} {:>9.2} {:>8} {:>8} {:>8}",
            b.name,
            b.concurrency,
            b.req_per_sec,
            b.mib_per_sec,
            fmt_us(b.p50_us),
            fmt_us(b.p95_us),
            fmt_us(b.p99_us),
        );
    }
    println!();
}

fn print_resource_report(
    idle: &ResourceStats,
    overall: &ResourceStats,
    phases: &[ResourceStats],
    upstream: &MockUpstream,
) {
    println!(
        "{}",
        style("--------------------------------------------------------------------------------")
            .dim()
    );
    println!(
        "{}",
        style(" 3. SERVER CPU & MEMORY USAGE OVERVIEW (/proc/<pid>)").bold()
    );
    println!(
        "{}",
        style("--------------------------------------------------------------------------------")
            .dim()
    );
    println!(
        " {:<38} {:>8} {:>9} {:>9} {:>9} {:>10} {:>10} {:>5}",
        style("Execution Phase").underlined(),
        style("Wall").underlined(),
        style("CPU Avg").underlined(),
        style("CPU Max").underlined(),
        style("CPU ms").underlined(),
        style("RSS Med").underlined(),
        style("RSS Max").underlined(),
        style("Thr").underlined(),
    );

    for p in phases {
        println!(
            " {:<38} {:>7.1}ms {:>8.1}% {:>8.1}% {:>7.0}ms {:>8.2}MB {:>8.2}MB {:>5}",
            p.phase_name,
            p.duration.as_secs_f64() * 1000.0,
            p.cpu_avg_pct,
            p.cpu_max_pct,
            p.cpu_time_ms,
            kb_to_mib(p.rss_p50_kb),
            kb_to_mib(p.rss_max_kb),
            p.max_threads,
        );
    }

    println!(
        " {}",
        style("──────────────────────────────────────────────────────────────────────────────")
            .dim()
    );
    println!(
        " {:<38} {:>7.1}ms {:>8.1}% {:>8.1}% {:>7.0}ms {:>8.2}MB {:>8.2}MB {:>5}",
        style(&overall.phase_name).bold(),
        overall.duration.as_secs_f64() * 1000.0,
        overall.cpu_avg_pct,
        overall.cpu_max_pct,
        overall.cpu_time_ms,
        kb_to_mib(overall.rss_p50_kb),
        kb_to_mib(overall.rss_max_kb),
        overall.max_threads,
    );

    let webhooks_count = upstream.webhooks.lock().map(|w| w.len()).unwrap_or(0);
    println!("\n {}", style("Server Telemetry Summary:").cyan().bold());
    println!(
        "  • Idle Memory Footprint (RSS) : {:.2} MiB",
        kb_to_mib(idle.rss_min_kb.max(1))
    );
    println!(
        "  • Median Memory Usage (RSS)   : {:.2} MiB (p95: {:.2} MiB | avg: {:.2} MiB)",
        kb_to_mib(overall.rss_p50_kb),
        kb_to_mib(overall.rss_p95_kb),
        kb_to_mib(overall.rss_avg_kb)
    );
    println!(
        "  • Peak Memory Usage (VmHWM)   : {:.2} MiB (Kernel High-Water Mark across 90 MiB LRU stress)",
        kb_to_mib(overall.hwm_kb)
    );
    println!(
        "  • Virtual Address Space       : {:.2} MiB",
        kb_to_mib(overall.vsize_max_kb)
    );
    println!(
        "  • Server CPU Utilization      : {:.1}% avg | {:.1}% p95 | {:.1}% peak ({:.0}ms total CPU time)",
        overall.cpu_avg_pct, overall.cpu_p95_pct, overall.cpu_max_pct, overall.cpu_time_ms
    );
    println!(
        "  • Server OS Threads & CtxtSw  : {}–{} threads | {} voluntary / {} non-voluntary switches",
        overall.min_threads,
        overall.max_threads,
        overall.voluntary_switches_delta,
        overall.nonvoluntary_switches_delta
    );
    println!(
        "  • Upstream Traffic Recorded   : {} searches, {} chapter scrapes, {} images, {} webhooks",
        upstream.search_hits.load(Ordering::Relaxed),
        upstream.chapter_hits.load(Ordering::Relaxed),
        upstream.image_hits.load(Ordering::Relaxed),
        webhooks_count
    );
}

fn kb_to_mib(kb: u64) -> f64 {
    (kb as f64) / 1024.0
}

fn fmt_us(us: u64) -> String {
    if us < 1_000 {
        format!("{}µs", us)
    } else if us < 1_000_000 {
        format!("{:.2}ms", (us as f64) / 1_000.0)
    } else {
        format!("{:.2}s", (us as f64) / 1_000_000.0)
    }
}

fn wait_until_server_ready(
    client: &Client,
    child: &mut Child,
    server_log: &Path,
) -> Result<String> {
    let deadline = Instant::now() + Duration::from_secs(5);
    let mut discovered_url: Option<String> = None;
    let mut last_err = String::new();

    while Instant::now() < deadline {
        if let Ok(Some(status)) = child.try_wait() {
            let logs = fs::read_to_string(server_log).unwrap_or_default();
            bail!(
                "ifetch --server exited prematurely with status {}. Server logs:\n{}",
                status,
                logs
            );
        }

        if discovered_url.is_none() {
            if let Ok(logs) = fs::read_to_string(server_log) {
                for line in logs.lines() {
                    if let Some(idx) = line.find("Server running on http://0.0.0.0:") {
                        let port_str =
                            line[idx + "Server running on http://0.0.0.0:".len()..].trim();
                        if let Ok(port) = port_str.parse::<u16>() {
                            discovered_url = Some(format!("http://127.0.0.1:{}", port));
                            break;
                        }
                    }
                }
            }
        }

        if let Some(ref url) = discovered_url {
            let probe_url = format!("{}/api/health-probe-404", url);
            match client.get(&probe_url).send() {
                Ok(resp) if resp.status().as_u16() == 404 => return Ok(url.clone()),
                Ok(resp) => last_err = format!("unexpected HTTP {}", resp.status()),
                Err(e) => last_err = e.to_string(),
            }
        }

        thread::sleep(Duration::from_millis(15));
    }
    let logs = fs::read_to_string(server_log).unwrap_or_default();
    bail!(
        "Timed out waiting for ifetch --server to accept connections (last err: {}). Server logs:\n{}",
        last_err,
        logs
    )
}

fn resolve_ifetch_binary(explicit: Option<PathBuf>) -> Result<PathBuf> {
    if let Some(p) = explicit {
        if p.exists() {
            return Ok(p.canonicalize()?);
        }
        bail!("Specified --bin path does not exist: {}", p.display());
    }

    let candidates = [
        Path::new("target/release/ifetch"),
        Path::new("../target/release/ifetch"),
        Path::new("target/debug/ifetch"),
        Path::new("../target/debug/ifetch"),
    ];

    for c in candidates {
        if c.exists() {
            return Ok(c.canonicalize()?);
        }
    }

    println!("  Building release binary of `ifetch`...");
    let status = Command::new("cargo")
        .args(["build", "--release"])
        .status()
        .context("Failed to invoke `cargo build --release`")?;
    if !status.success() {
        bail!("`cargo build --release` failed");
    }

    for c in candidates {
        if c.exists() {
            return Ok(c.canonicalize()?);
        }
    }

    bail!("Could not locate compiled `ifetch` binary")
}
