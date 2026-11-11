use crate::{client::Client, monitor::ProcessMonitor};
use anyhow::Result;
use std::{
    fs::File,
    io::Write,
    path::Path,
    sync::{
        Arc, Mutex,
        atomic::{AtomicU64, AtomicUsize, Ordering},
    },
    thread,
    time::{Duration, Instant},
};
use zip::write::SimpleFileOptions;

#[derive(Debug, Clone)]
#[allow(dead_code)]
pub struct BenchmarkResult {
    pub name: &'static str,
    pub concurrency: usize,
    pub total_requests: usize,
    pub failed_requests: usize,
    pub total_bytes: u64,
    pub elapsed: Duration,
    pub req_per_sec: f64,
    pub mib_per_sec: f64,
    pub min_us: u64,
    pub avg_us: u64,
    pub p50_us: u64,
    pub p95_us: u64,
    pub p99_us: u64,
    pub max_us: u64,
}

pub fn run_all_benchmarks(
    server_url: &str,
    output_dir: &Path,
    monitor: &ProcessMonitor,
) -> Result<Vec<BenchmarkResult>> {
    let mut results = Vec::new();
    let manga_dir = output_dir.join("Cyber Blade & Steel");

    // Prepare a 50-page chapter (64 KiB per page) for sequential cold/warm reader benchmarks
    let ch50_path = manga_dir.join("Cyber Blade & Steel - Chapter 050.cbz");
    create_benchmark_cbz(&ch50_path, 50, 64 * 1024)?;

    // Benchmark 1: Metadata & Chapter List Route Throughput
    {
        let marker = monitor.mark_phase_start();
        let url_meta = format!("{}/api/manga/cyber-blade.101", server_url);
        let url_chaps = format!("{}/api/manga/cyber-blade.101/chapters", server_url);
        let res = run_concurrent_load(
            "Metadata & Chapter Scan (/api/manga/{id}[/chapters])",
            8,
            400,
            move |client, idx| {
                let target = if idx % 2 == 0 { &url_meta } else { &url_chaps };
                let resp = client.get(target).send()?.error_for_status()?;
                Ok(resp.bytes()?.len() as u64)
            },
        );
        monitor.record_phase("Bench 1: Metadata & Chapter Scan", marker);
        results.push(res);
    }

    // Benchmark 2: Cached Chapter Page Index Throughput
    {
        let marker = monitor.mark_phase_start();
        let url_pages = format!("{}/api/chapter/cyber-blade.101::1/pages", server_url);
        let res = run_concurrent_load(
            "Cached Page Index (/api/chapter/{id}/pages)",
            8,
            800,
            move |client, _| {
                let resp = client.get(&url_pages).send()?.error_for_status()?;
                Ok(resp.bytes()?.len() as u64)
            },
        );
        monitor.record_phase("Bench 2: Cached Page Index", marker);
        results.push(res);
    }

    // Benchmark 3: Sequential Cold CBZ Reader (with 5-page sliding prefetch)
    {
        let marker = monitor.mark_phase_start();
        let base = server_url.to_string();
        let res = run_concurrent_load(
            "Sequential Reader: Cold CBZ + 5-Page Prefetch (50x 64KiB)",
            1,
            50,
            move |client, idx| {
                let page_num = (idx % 50) + 1;
                let url = format!("{}/api/image/cyber-blade.101::50/{:03}.jpg", base, page_num);
                let resp = client.get(&url).send()?.error_for_status()?;
                Ok(resp.bytes()?.len() as u64)
            },
        );
        monitor.record_phase("Bench 3: Cold CBZ + Prefetch", marker);
        results.push(res);
    }

    // Benchmark 4: Warm ImageLruCache Concurrent Zero-Copy Serving
    {
        let marker = monitor.mark_phase_start();
        let base = server_url.to_string();
        let res = run_concurrent_load(
            "Warm ImageLruCache Zero-Copy Serving (64KiB pages)",
            12,
            1200,
            move |client, idx| {
                let page_num = (idx % 50) + 1;
                let url = format!("{}/api/image/cyber-blade.101::50/{:03}.jpg", base, page_num);
                let resp = client.get(&url).send()?.error_for_status()?;
                Ok(resp.bytes()?.len() as u64)
            },
        );
        monitor.record_phase("Bench 4: Warm ImageLruCache (12 threads)", marker);
        results.push(res);
    }

    // Benchmark 5: 64 MiB ImageLruCache Eviction Pressure Test (streaming 90 MiB of distinct 1 MiB pages)
    {
        let ch90_path = manga_dir.join("Cyber Blade & Steel - Chapter 090.cbz");
        let ch91_path = manga_dir.join("Cyber Blade & Steel - Chapter 091.cbz");
        let ch92_path = manga_dir.join("Cyber Blade & Steel - Chapter 092.cbz");
        create_benchmark_cbz(&ch90_path, 30, 1024 * 1024)?;
        create_benchmark_cbz(&ch91_path, 30, 1024 * 1024)?;
        create_benchmark_cbz(&ch92_path, 30, 1024 * 1024)?;

        let marker = monitor.mark_phase_start();
        let base = server_url.to_string();
        let res = run_concurrent_load(
            "LRU Eviction Stress (90x 1MiB Unique Pages > 64MiB Cap)",
            4,
            90,
            move |client, idx| {
                let ch = 90 + (idx / 30);
                let page = (idx % 30) + 1;
                let url = format!("{}/api/image/cyber-blade.101::{}/{:03}.jpg", base, ch, page);
                let resp = client.get(&url).send()?.error_for_status()?;
                Ok(resp.bytes()?.len() as u64)
            },
        );
        monitor.record_phase("Bench 5: 64MiB LRU Eviction Stress", marker);
        results.push(res);
    }

    Ok(results)
}

fn run_concurrent_load<F>(
    name: &'static str,
    concurrency: usize,
    total_requests: usize,
    task: F,
) -> BenchmarkResult
where
    F: Fn(&Client, usize) -> Result<u64> + Send + Sync + 'static,
{
    let task = Arc::new(task);
    let next_idx = Arc::new(AtomicUsize::new(0));
    let failed = Arc::new(AtomicUsize::new(0));
    let total_bytes = Arc::new(AtomicU64::new(0));
    let latencies_us = Arc::new(Mutex::new(Vec::with_capacity(total_requests)));

    let start = Instant::now();
    let mut handles = Vec::with_capacity(concurrency);

    for _ in 0..concurrency {
        let task = Arc::clone(&task);
        let next_idx = Arc::clone(&next_idx);
        let failed = Arc::clone(&failed);
        let total_bytes = Arc::clone(&total_bytes);
        let latencies_us = Arc::clone(&latencies_us);

        let handle = thread::spawn(move || {
            let client = Client::builder()
                .timeout(Duration::from_secs(15))
                .build()
                .expect("Failed to build benchmark HTTP client");
            let mut local_lats = Vec::with_capacity((total_requests / concurrency) + 16);

            loop {
                let idx = next_idx.fetch_add(1, Ordering::Relaxed);
                if idx >= total_requests {
                    break;
                }

                let t0 = Instant::now();
                match task(&client, idx) {
                    Ok(bytes) => {
                        let us = t0.elapsed().as_micros() as u64;
                        local_lats.push(us);
                        total_bytes.fetch_add(bytes, Ordering::Relaxed);
                    }
                    Err(_) => {
                        failed.fetch_add(1, Ordering::Relaxed);
                    }
                }
            }

            if let Ok(mut guard) = latencies_us.lock() {
                guard.extend(local_lats);
            }
        });
        handles.push(handle);
    }

    for h in handles {
        let _ = h.join();
    }

    let elapsed = start.elapsed();
    let elapsed_secs = elapsed.as_secs_f64().max(0.000_001);
    let bytes = total_bytes.load(Ordering::Relaxed);
    let failed_count = failed.load(Ordering::Relaxed);

    let mut lats = latencies_us.lock().unwrap().clone();
    lats.sort_unstable();

    let (min_us, avg_us, p50_us, p95_us, p99_us, max_us) = if lats.is_empty() {
        (0, 0, 0, 0, 0, 0)
    } else {
        let len = lats.len();
        let sum: u64 = lats.iter().sum();
        let p = |q: f64| lats[((len as f64 * q) as usize).min(len - 1)];
        (
            lats[0],
            sum / (len as u64),
            p(0.50),
            p(0.95),
            p(0.99),
            lats[len - 1],
        )
    };

    BenchmarkResult {
        name,
        concurrency,
        total_requests,
        failed_requests: failed_count,
        total_bytes: bytes,
        elapsed,
        req_per_sec: (total_requests as f64) / elapsed_secs,
        mib_per_sec: (bytes as f64 / (1024.0 * 1024.0)) / elapsed_secs,
        min_us,
        avg_us,
        p50_us,
        p95_us,
        p99_us,
        max_us,
    }
}

fn create_benchmark_cbz(path: &Path, pages: usize, page_size_bytes: usize) -> Result<()> {
    let file = File::create(path)?;
    let mut zip = zip::ZipWriter::new(file);
    let opts = SimpleFileOptions::default().compression_method(zip::CompressionMethod::Stored);

    zip.start_file("ComicInfo.xml", opts)?;
    zip.write_all(b"<?xml version=\"1.0\"?><ComicInfo><Series>Cyber Blade &amp; &lt;Steel&gt;</Series></ComicInfo>")?;

    let mut img_buf = Vec::with_capacity(page_size_bytes);
    img_buf.extend_from_slice(b"\xff\xd8\xff\xe0\x00\x10JFIF\x00");
    img_buf.resize(page_size_bytes, 0xA5);

    for i in 1..=pages {
        zip.start_file(format!("{:03}.jpg", i), opts)?;
        zip.write_all(&img_buf)?;
    }

    zip.finish()?;
    Ok(())
}
