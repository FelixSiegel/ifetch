use crate::{client::Client, mock_upstream::MockUpstream, monitor::ProcessMonitor};
use anyhow::{Context, Result, ensure};
use http::StatusCode;
use serde::Deserialize;
use std::{
    fs::{self, File},
    io::{Read, Write},
    path::Path,
    process::Command,
    sync::atomic::Ordering,
    thread,
    time::{Duration, Instant},
};
use zip::write::SimpleFileOptions;

#[derive(Debug, Clone)]
pub struct TestCaseResult {
    pub category: &'static str,
    pub name: &'static str,
    pub passed: bool,
    pub duration: Duration,
    pub details: String,
}

#[derive(Debug, Deserialize)]
#[allow(dead_code)]
struct ApiManga {
    id: String,
    title: String,
    url: String,
    #[serde(default)]
    cover_url: String,
    #[serde(default)]
    status: String,
    #[serde(default)]
    description: String,
    #[serde(default)]
    genres: Vec<String>,
    #[serde(default)]
    authors: Vec<String>,
    #[serde(default)]
    alt_names: Vec<String>,
}

#[derive(Debug, Deserialize)]
#[allow(dead_code)]
struct ApiChapter {
    number: String,
    label: String,
    url: String,
    #[serde(default)]
    downloaded: bool,
}

pub fn run_all_tests(
    client: &Client,
    server_url: &str,
    upstream: &MockUpstream,
    output_dir: &Path,
    config_dir: &Path,
    ifetch_bin: &Path,
    monitor: &ProcessMonitor,
) -> Vec<TestCaseResult> {
    let mut results = Vec::new();

    // Phase 1: Route Security & Input Validation
    let phase_marker = monitor.mark_phase_start();
    run_case(
        &mut results,
        "Routing & Security",
        "Unknown route returns 404 Not Found",
        || {
            let res = client
                .get(format!("{}/api/does-not-exist", server_url))
                .send()?;
            ensure!(
                res.status() == StatusCode::NOT_FOUND,
                "Expected 404, got {}",
                res.status()
            );
            Ok("GET /api/does-not-exist -> 404".to_string())
        },
    );

    run_case(
        &mut results,
        "Routing & Security",
        "Search without 'q' parameter returns 400 Bad Request",
        || {
            let res = client.get(format!("{}/api/search", server_url)).send()?;
            ensure!(
                res.status() == StatusCode::BAD_REQUEST,
                "Expected 400, got {}",
                res.status()
            );
            Ok("GET /api/search -> 400 Missing q".to_string())
        },
    );

    run_case(
        &mut results,
        "Routing & Security",
        "Path traversal & malformed IDs are rejected with 400",
        || {
            let bad_urls = [
                "/api/manga/..::1",
                "/api/manga/..",
                "/api/manga/../chapters",
                "/api/manga/../download",
                "/api/chapter/invalid-no-separator/pages",
                "/api/chapter/..::1/pages",
                "/api/chapter/cyber-blade.101::../pages",
                "/api/image/cyber-blade.101::1/..",
                "/api/image/cyber-blade.101::1/..%5Csecret",
                "/api/image/..::1/001.jpg",
            ];
            for path in bad_urls {
                let res = client.get(format!("{}{}", server_url, path)).send()?;
                ensure!(
                    res.status() == StatusCode::BAD_REQUEST
                        || res.status() == StatusCode::NOT_FOUND,
                    "Path {} was not rejected (got {})",
                    path,
                    res.status()
                );
            }
            Ok(format!(
                "Verified {} traversal/malformed vectors blocked",
                bad_urls.len()
            ))
        },
    );

    run_case(
        &mut results,
        "Routing & Security",
        "Missing chapter pages and images return 404 Not Found",
        || {
            let res0 = client
                .get(format!("{}/api/manga/non-existent.999", server_url))
                .send()?;
            ensure!(
                res0.status() == StatusCode::NOT_FOUND,
                "Expected 404 for non-existent manga details, got {}",
                res0.status()
            );

            let res0_ch = client
                .get(format!(
                    "{}/api/manga/non-existent.999/chapters",
                    server_url
                ))
                .send()?;
            ensure!(
                res0_ch.status() == StatusCode::NOT_FOUND,
                "Expected 404 for non-existent manga chapters, got {}",
                res0_ch.status()
            );

            let res1 = client
                .get(format!(
                    "{}/api/chapter/direct-manga.777::999/pages",
                    server_url
                ))
                .send()?;
            ensure!(
                res1.status() == StatusCode::NOT_FOUND,
                "Expected 404 for missing pages"
            );

            let res2 = client
                .get(format!(
                    "{}/api/image/direct-manga.777::999/001.jpg",
                    server_url
                ))
                .send()?;
            ensure!(
                res2.status() == StatusCode::NOT_FOUND,
                "Expected 404 for missing chapter image"
            );
            Ok("Missing chapter & image requests -> 404".to_string())
        },
    );
    monitor.record_phase("1. Route Security & Validation", phase_marker);

    // Phase 2: Search & Upstream Scraping
    let phase_marker = monitor.mark_phase_start();
    run_case(
        &mut results,
        "Search & Scraping",
        "Multi-result search parses items, covers, and deduplicates links",
        || {
            let res = client
                .get(format!("{}/api/search?q=cyber", server_url))
                .send()?
                .error_for_status()?;
            let mangas: Vec<ApiManga> = res.json()?;
            ensure!(
                mangas.len() == 2,
                "Expected 2 deduplicated results, got {}",
                mangas.len()
            );
            ensure!(
                mangas[0].id == "cyber-blade.101",
                "Unexpected first ID: {}",
                mangas[0].id
            );
            ensure!(
                mangas[0].title == "Cyber Blade & <Steel>",
                "Unexpected title: {}",
                mangas[0].title
            );
            ensure!(
                mangas[0].status == "Ongoing",
                "Unexpected status: {}",
                mangas[0].status
            );
            ensure!(
                !mangas[0].cover_url.is_empty(),
                "Expected non-empty data-src cover_url"
            );
            ensure!(
                mangas[1].id == "padding-saga.202",
                "Unexpected second ID: {}",
                mangas[1].id
            );
            Ok(format!(
                "Parsed {} unique series from search HTML",
                mangas.len()
            ))
        },
    );

    run_case(
        &mut results,
        "Search & Scraping",
        "Direct-hit search follows 302 redirect to single manga page",
        || {
            let res = client
                .get(format!("{}/api/search?q=direct-hit", server_url))
                .send()?
                .error_for_status()?;
            let mangas: Vec<ApiManga> = res.json()?;
            ensure!(
                mangas.len() == 1,
                "Expected 1 direct-hit manga, got {}",
                mangas.len()
            );
            ensure!(
                mangas[0].id == "direct-manga.777",
                "Got ID {}",
                mangas[0].id
            );
            ensure!(
                mangas[0].title == "Direct Hit Manga",
                "Got title {}",
                mangas[0].title
            );
            Ok("302 redirect -> single Manga object".to_string())
        },
    );

    run_case(
        &mut results,
        "Search & Scraping",
        "Empty search query match returns empty JSON array []",
        || {
            let res = client
                .get(format!("{}/api/search?q=no-match-xyz", server_url))
                .send()?
                .error_for_status()?;
            let mangas: Vec<ApiManga> = res.json()?;
            ensure!(
                mangas.is_empty(),
                "Expected 0 results, got {}",
                mangas.len()
            );
            Ok("0 results -> []".to_string())
        },
    );

    run_case(
        &mut results,
        "Search & Scraping",
        "Popular manga returns front page hot manga with page handling",
        || {
            let res = client
                .get(format!("{}/api/popular", server_url))
                .send()?
                .error_for_status()?;
            let popular_mangas: Vec<ApiManga> = res.json()?;
            ensure!(
                popular_mangas.len() == 1,
                "Expected 1 hot manga, got {}",
                popular_mangas.len()
            );
            ensure!(
                popular_mangas[0].id == "cyber-blade.101",
                "Unexpected hot manga ID: {}",
                popular_mangas[0].id
            );

            // Page > 1 returns empty array
            let res_p2 = client
                .get(format!("{}/api/popular?page=2", server_url))
                .send()?
                .error_for_status()?;
            let p2_mangas: Vec<ApiManga> = res_p2.json()?;
            ensure!(
                p2_mangas.is_empty(),
                "Expected empty array for page 2 popular manga"
            );

            Ok("GET /api/popular verified with page handling".to_string())
        },
    );

    run_case(
        &mut results,
        "Search & Scraping",
        "Latest manga returns updated manga list and supports pagination",
        || {
            let res = client
                .get(format!("{}/api/latest", server_url))
                .send()?
                .error_for_status()?;
            let latest_mangas: Vec<ApiManga> = res.json()?;
            ensure!(
                latest_mangas.len() == 2,
                "Expected 2 latest manga, got {}",
                latest_mangas.len()
            );
            ensure!(
                latest_mangas[0].id == "cyber-blade.101",
                "Unexpected first latest manga ID: {}",
                latest_mangas[0].id
            );

            // Page 2 request
            let res_p2 = client
                .get(format!("{}/api/latest?page=2", server_url))
                .send()?
                .error_for_status()?;
            let p2_mangas: Vec<ApiManga> = res_p2.json()?;
            ensure!(
                p2_mangas.len() == 2,
                "Expected 2 latest manga on page 2, got {}",
                p2_mangas.len()
            );

            // Out-of-bounds page (upstream 404) returns []
            let res_oob = client
                .get(format!("{}/api/latest?page=20000", server_url))
                .send()?
                .error_for_status()?;
            let oob_mangas: Vec<ApiManga> = res_oob.json()?;
            ensure!(
                oob_mangas.is_empty(),
                "Expected empty array for out-of-bounds page 20000"
            );

            Ok(
                "GET /api/latest verified with pagination and out-of-bounds 404 handling"
                    .to_string(),
            )
        },
    );

    run_case(
        &mut results,
        "Search & Scraping",
        "New manga returns newly added manga list and supports pagination",
        || {
            let res = client
                .get(format!("{}/api/new", server_url))
                .send()?
                .error_for_status()?;
            let new_mangas: Vec<ApiManga> = res.json()?;
            ensure!(
                new_mangas.len() == 2,
                "Expected 2 new manga, got {}",
                new_mangas.len()
            );
            ensure!(
                new_mangas[0].id == "cyber-blade.101",
                "Unexpected first new manga ID: {}",
                new_mangas[0].id
            );

            // Page 2 request
            let res_p2 = client
                .get(format!("{}/api/new?page=2", server_url))
                .send()?
                .error_for_status()?;
            let p2_mangas: Vec<ApiManga> = res_p2.json()?;
            ensure!(
                p2_mangas.len() == 2,
                "Expected 2 new manga on page 2, got {}",
                p2_mangas.len()
            );

            Ok("GET /api/new verified with pagination".to_string())
        },
    );
    monitor.record_phase("2. Search & Scraping", phase_marker);

    // Phase 3: Manga Details & Remote TTL Cache Proof
    let phase_marker = monitor.mark_phase_start();
    run_case(
        &mut results,
        "Caching & Metadata",
        "GET /api/manga/{id} scrapes full metadata and caches in TTL cache",
        || {
            let hits_before = upstream.manga_hit_count("cyber-blade.101");
            let manga: ApiManga = client
                .get(format!("{}/api/manga/cyber-blade.101", server_url))
                .send()?
                .error_for_status()?
                .json()?;

            ensure!(manga.title == "Cyber Blade & <Steel>", "Title mismatch");
            ensure!(
                manga.genres == vec!["Action", "Sci-Fi"],
                "Genres mismatch: {:?}",
                manga.genres
            );
            ensure!(
                manga.authors == vec!["Ada Lovelace", "Alan Turing"],
                "Authors mismatch: {:?}",
                manga.authors
            );
            ensure!(
                manga.alt_names == vec!["Cyber Blade", "Blade of Steel"],
                "Alt names mismatch: {:?}",
                manga.alt_names
            );

            let hits_after_first = upstream.manga_hit_count("cyber-blade.101");
            ensure!(
                hits_after_first == hits_before + 1,
                "Expected upstream hit count to increment by 1"
            );

            // Issue 5 more requests; all must be served from ServerCache::remote_manga
            for _ in 0..5 {
                let cached: ApiManga = client
                    .get(format!("{}/api/manga/cyber-blade.101", server_url))
                    .send()?
                    .error_for_status()?
                    .json()?;
                ensure!(cached.id == "cyber-blade.101", "Cached ID mismatch");
            }

            let hits_after_burst = upstream.manga_hit_count("cyber-blade.101");
            ensure!(
                hits_after_burst == hits_after_first,
                "Remote TTL cache miss! Upstream hits grew from {} to {}",
                hits_after_first,
                hits_after_burst
            );
            Ok("1 upstream fetch for 6 requests (5 TTL cache hits)".to_string())
        },
    );
    monitor.record_phase("3. Metadata & TTL Cache", phase_marker);

    // Phase 4: Read-Only Chapters, Explicit POST /download, Deduplication & CBZ Creation
    let phase_marker = monitor.mark_phase_start();
    run_case(
        &mut results,
        "Downloader & CBZ",
        "Initial /chapters is read-only, POST /download triggers queued chapters, and completes download",
        || {
            // 1. Verify that GET /chapters is strictly read-only and idempotent
            let res = client
                .get(format!("{}/api/manga/cyber-blade.101/chapters", server_url))
                .send()?;
            ensure!(
                res.status() == StatusCode::OK,
                "Expected 200 OK from read-only GET /chapters, got {}",
                res.status()
            );
            let chaps: Vec<ApiChapter> = res.json()?;
            ensure!(
                chaps.len() == 3,
                "Expected 3 remote chapters, got {}",
                chaps.len()
            );
            ensure!(
                chaps.iter().all(|c| !c.downloaded),
                "Expected all chapters to have downloaded=false initially"
            );

            // Test query parameter filtering
            let downloaded: Vec<ApiChapter> = client
                .get(format!(
                    "{}/api/manga/cyber-blade.101/chapters?filter=downloaded",
                    server_url
                ))
                .send()?
                .json()?;
            ensure!(
                downloaded.is_empty(),
                "Expected 0 downloaded chapters before download trigger"
            );

            let missing: Vec<ApiChapter> = client
                .get(format!(
                    "{}/api/manga/cyber-blade.101/chapters?filter=missing",
                    server_url
                ))
                .send()?
                .json()?;
            ensure!(
                missing.len() == 3,
                "Expected 3 missing chapters before download trigger"
            );

            // Verify telemetry before triggering download
            let pre_telemetry: serde_json::Value = client
                .get(format!("{}/api/manga/cyber-blade.101/download", server_url))
                .send()?
                .json()?;
            ensure!(
                pre_telemetry["is_downloading"] == false,
                "Expected is_downloading=false before trigger"
            );

            let all_telemetry: serde_json::Value = client
                .get(format!("{}/api/downloads", server_url))
                .send()?
                .json()?;
            ensure!(
                all_telemetry["active_downloads"]
                    .as_array()
                    .map(|a| a.is_empty())
                    .unwrap_or(false),
                "Expected active_downloads to be empty before trigger"
            );

            // Ensure no CBZ files exist yet
            let manga_dir = output_dir.join("Cyber Blade & Steel");
            ensure!(
                !manga_dir
                    .join("Cyber Blade & Steel - Chapter 001.cbz")
                    .exists(),
                "Chapter 001.cbz should not exist before download trigger"
            );

            // Verify that informational GET requests did not insert cyber-blade.101 into library.db before download trigger
            let db_path = config_dir.join("library.db");
            if db_path.exists() {
                let conn = rusqlite::Connection::open(&db_path)?;
                let exists: bool = conn
                    .prepare("SELECT 1 FROM mangas WHERE id = 'cyber-blade.101'")?
                    .exists([])?;
                ensure!(
                    !exists,
                    "cyber-blade.101 should not be in library.db before explicit download trigger"
                );
            }

            // Verify invalid filter returns 400 Bad Request
            let inv_res = client
                .get(format!(
                    "{}/api/manga/cyber-blade.101/chapters?filter=invalid_filter",
                    server_url
                ))
                .send()?;
            ensure!(
                inv_res.status() == StatusCode::BAD_REQUEST,
                "Expected 400 Bad Request for invalid filter"
            );

            // Verify invalid download payloads return 400 Bad Request
            let bad_json_res = client
                .post(format!("{}/api/manga/cyber-blade.101/download", server_url))
                .header("Content-Type", "application/json")
                .body("invalid json {")
                .send()?;
            ensure!(
                bad_json_res.status() == StatusCode::BAD_REQUEST,
                "Expected 400 Bad Request for malformed JSON"
            );

            let bad_range_res = client
                .post(format!("{}/api/manga/cyber-blade.101/download", server_url))
                .json(&serde_json::json!({ "range": "10-1" }))
                .send()?;
            ensure!(
                bad_range_res.status() == StatusCode::BAD_REQUEST,
                "Expected 400 Bad Request for descending range"
            );

            // 2. Trigger explicit download via POST /api/manga/{id}/download
            let dl_res = client
                .post(format!("{}/api/manga/cyber-blade.101/download", server_url))
                .send()?;
            ensure!(
                dl_res.status() == StatusCode::ACCEPTED,
                "Expected 202 Accepted from POST /download, got {}",
                dl_res.status()
            );
            let dl_json: serde_json::Value = dl_res.json()?;
            ensure!(
                dl_json["status"] == "queued",
                "Expected status=queued, got {:?}",
                dl_json["status"]
            );
            ensure!(
                dl_json["manga_id"] == "cyber-blade.101",
                "Expected manga_id=cyber-blade.101"
            );
            ensure!(
                dl_json["queued_count"] == 3,
                "Expected queued_count=3, got {:?}",
                dl_json["queued_count"]
            );

            // Fire 4 concurrent requests to verify active_downloads deduplication guard
            for _ in 0..4 {
                let r = client
                    .post(format!("{}/api/manga/cyber-blade.101/download", server_url))
                    .send()?;
                ensure!(
                    r.status() == StatusCode::ACCEPTED,
                    "Expected 202 Accepted during concurrent trigger: {}",
                    r.status()
                );
                let dup: serde_json::Value = r.json()?;
                ensure!(
                    dup["queued_count"] == 0,
                    "Expected queued_count=0 for duplicate trigger, got {:?}",
                    dup["queued_count"]
                );
            }

            // Poll until all 3 chapters (1, 2, 10.5) are downloaded
            let deadline = Instant::now() + Duration::from_secs(15);
            let mut chapters: Vec<ApiChapter> = Vec::new();
            while Instant::now() < deadline {
                let r = client
                    .get(format!(
                        "{}/api/manga/cyber-blade.101/chapters?filter=downloaded",
                        server_url
                    ))
                    .send()?;
                if r.status() == StatusCode::OK {
                    let list: Vec<ApiChapter> = r.json()?;
                    if list.len() == 3 {
                        chapters = list;
                        break;
                    }
                }
                thread::sleep(Duration::from_millis(50));
            }

            ensure!(
                chapters.len() == 3,
                "Timed out waiting for 3 chapters to download (got {})",
                chapters.len()
            );
            ensure!(
                chapters.iter().all(|c| c.downloaded),
                "Expected all downloaded chapters to have downloaded=true"
            );
            let nums: Vec<&str> = chapters.iter().map(|c| c.number.as_str()).collect();
            ensure!(
                nums == vec!["1", "2", "10.5"],
                "Chapters not sorted or fractional Decimal corrupted: {:?}",
                nums
            );

            // Verify ?sv=3 mirror fallback and transient 502 image retry occurred
            ensure!(
                upstream.mirror_sv3_hits.load(Ordering::SeqCst) >= 1,
                "Expected ?sv=3 mirror fallback to be used for Chapter 2"
            );
            ensure!(
                upstream.transient_retry_hits.load(Ordering::SeqCst) >= 2,
                "Expected fetch_single_image retry on HTTP 502"
            );

            // Verify DELETE /api/manga/{id}/download cancellation endpoint
            let cancel_res = client
                .delete(format!("{}/api/manga/cyber-blade.101/download", server_url))
                .send()?;
            ensure!(
                cancel_res.status() == StatusCode::OK,
                "Expected 200 OK from DELETE /download, got {}",
                cancel_res.status()
            );
            let cancel_json: serde_json::Value = cancel_res.json()?;
            ensure!(
                cancel_json["status"] == "cancelled",
                "Expected status=cancelled"
            );

            Ok("Downloaded Ch 1, 2 (?sv=3 mirror + 502 retry), 10.5 (fractional)".to_string())
        },
    );

    run_case(
        &mut results,
        "Downloader & CBZ",
        "Verify CBZ archives, atomic .part cleanup, magic-byte extensions, and ComicInfo.xml escaping",
        || {
            let manga_dir = output_dir.join("Cyber Blade & Steel");
            ensure!(
                manga_dir.exists(),
                "Manga directory {:?} does not exist",
                manga_dir
            );

            // Ensure no leftover .cbz.part files exist
            for entry in fs::read_dir(&manga_dir)? {
                let name = entry?.file_name().into_string().unwrap_or_default();
                ensure!(
                    !name.ends_with(".part"),
                    "Found leftover partial file: {}",
                    name
                );
            }

            let ch1_cbz = manga_dir.join("Cyber Blade & Steel - Chapter 001.cbz");
            let ch2_cbz = manga_dir.join("Cyber Blade & Steel - Chapter 002.cbz");
            let ch10_5_cbz = manga_dir.join("Cyber Blade & Steel - Chapter 010.5.cbz");
            ensure!(ch1_cbz.exists(), "Missing Ch 001 CBZ");
            ensure!(ch2_cbz.exists(), "Missing Ch 002 CBZ");
            ensure!(ch10_5_cbz.exists(), "Missing Ch 010.5 CBZ");

            let file = File::open(&ch1_cbz)?;
            let mut archive = zip::ZipArchive::new(file)?;

            // Check ComicInfo.xml escaping
            let mut xml = String::new();
            archive
                .by_name("ComicInfo.xml")
                .context("ComicInfo.xml missing in CBZ")?
                .read_to_string(&mut xml)?;
            ensure!(
                xml.contains("<Series>Cyber Blade &amp; &lt;Steel&gt;</Series>"),
                "ComicInfo.xml Series not properly XML-escaped: {}",
                xml
            );
            ensure!(
                xml.contains("A &amp; B &lt; C &gt; D"),
                "ComicInfo.xml Summary not properly XML-escaped"
            );

            // Check all 8 page filenames & extensions (jpg, png, webp, gif, avif)
            let expected_entries = [
                "001.jpg", "002.png", "003.webp", "004.gif", "005.avif", "006.jpg", "007.jpg",
                "008.jpg",
            ];
            for name in expected_entries {
                ensure!(
                    archive.by_name(name).is_ok(),
                    "Expected entry '{}' in Chapter 1 CBZ",
                    name
                );
            }

            // Verify Discord webhooks (Start + Complete)
            let hooks = upstream.webhooks.lock().unwrap();
            ensure!(
                hooks.len() >= 2,
                "Expected at least 2 Discord webhook notifications (Start + Complete), got {}",
                hooks.len()
            );

            Ok("3 CBZs verified, XML escaped, 5 image formats & webhooks validated".to_string())
        },
    );
    monitor.record_phase("4. Background Download & CBZ Build", phase_marker);

    // Phase 5: Chapter Pages, Image Serving, Magic-Byte MIME & 5-Page Prefetch Proof
    let phase_marker = monitor.mark_phase_start();
    run_case(
        &mut results,
        "Image Cache & Prefetch",
        "Deterministic proof of chapter_pages cache, ImageLruCache hit, and 5-page prefetch window",
        || {
            let pages: Vec<String> = client
                .get(format!(
                    "{}/api/chapter/cyber-blade.101::1/pages",
                    server_url
                ))
                .send()?
                .error_for_status()?
                .json()?;
            ensure!(pages.len() == 8, "Expected 8 pages, got {}", pages.len());
            ensure!(
                pages[0] == "/api/image/cyber-blade.101::1/001.jpg",
                "Unexpected first page URL: {}",
                pages[0]
            );

            // Fetch page 1 (001.jpg). This caches 001.jpg and prefetches the next 5 pages (002..=006)
            // into ImageLruCache, leaving 007.jpg and 008.jpg uncached!
            let res_p1 = client
                .get(format!("{}{}", server_url, pages[0]))
                .send()?
                .error_for_status()?;
            ensure!(
                res_p1
                    .headers()
                    .get("Content-Type")
                    .and_then(|v| v.to_str().ok())
                    == Some("image/jpeg"),
                "Expected image/jpeg for 001.jpg"
            );
            ensure!(
                res_p1
                    .headers()
                    .get("Cache-Control")
                    .and_then(|v| v.to_str().ok())
                    == Some("public, max-age=31536000, immutable"),
                "Missing immutable Cache-Control header"
            );
            let p1_bytes = res_p1.bytes()?;
            ensure!(
                p1_bytes.len() == 4096 && p1_bytes.starts_with(b"\xff\xd8\xff"),
                "Invalid JPEG bytes"
            );

            // Temporarily hide the CBZ file on disk to prove pages 1..=6 are served purely from RAM cache!
            let manga_dir = output_dir.join("Cyber Blade & Steel");
            let cbz_path = manga_dir.join("Cyber Blade & Steel - Chapter 001.cbz");
            let hidden_path = manga_dir.join("Cyber Blade & Steel - Chapter 001.cbz.hidden");
            fs::rename(&cbz_path, &hidden_path)?;

            let proof_result = (|| -> Result<()> {
                // 1. /pages must still succeed from ServerCache::chapter_pages
                let cached_pages: Vec<String> = client
                    .get(format!(
                        "{}/api/chapter/cyber-blade.101::1/pages",
                        server_url
                    ))
                    .send()?
                    .error_for_status()?
                    .json()?;
                ensure!(
                    cached_pages.len() == 8,
                    "chapter_pages cache failed when CBZ hidden"
                );

                // 2. Pages 001..=006 (page 1 + 5 prefetched pages) MUST succeed from ImageLruCache
                //    with their exact MIME types!
                let expected_mimes = [
                    ("001.jpg", "image/jpeg"),
                    ("002.png", "image/png"),
                    ("003.webp", "image/webp"),
                    ("004.gif", "image/gif"),
                    ("005.avif", "image/avif"),
                    ("006.jpg", "image/jpeg"),
                ];
                for (fname, expected_mime) in expected_mimes {
                    let r = client
                        .get(format!(
                            "{}/api/image/cyber-blade.101::1/{}",
                            server_url, fname
                        ))
                        .send()?;
                    ensure!(
                        r.status() == StatusCode::OK,
                        "Expected cached/prefetched page {} to be served from RAM while CBZ is hidden, got {}",
                        fname,
                        r.status()
                    );
                    let ct = r
                        .headers()
                        .get("Content-Type")
                        .and_then(|v| v.to_str().ok())
                        .unwrap_or("");
                    ensure!(
                        ct == expected_mime,
                        "Expected MIME {} for {}, got {}",
                        expected_mime,
                        fname,
                        ct
                    );
                    let b = r.bytes()?;
                    ensure!(
                        b.len() == 4096,
                        "Expected 4096 bytes for {}, got {}",
                        fname,
                        b.len()
                    );
                }

                // 3. Page 007.jpg (beyond PREFETCH_PAGES = 5) was NOT prefetched, so without the CBZ on disk
                //    it must fail with 404/500!
                let r_uncached = client
                    .get(format!(
                        "{}/api/image/cyber-blade.101::1/007.jpg",
                        server_url
                    ))
                    .send()?;
                ensure!(
                    !r_uncached.status().is_success(),
                    "Expected uncached page 007.jpg to miss RAM cache while CBZ is hidden"
                );
                Ok(())
            })();

            // Always restore the CBZ file before propagating any error
            fs::rename(&hidden_path, &cbz_path)?;
            proof_result?;

            // Now that the CBZ is back on disk, 007.jpg must succeed and prefetch 008.jpg
            let r7 = client
                .get(format!(
                    "{}/api/image/cyber-blade.101::1/007.jpg",
                    server_url
                ))
                .send()?
                .error_for_status()?;
            ensure!(
                r7.bytes()?.len() == 4096,
                "007.jpg failed after restoring CBZ"
            );

            // Requesting a non-existent image inside a valid CBZ returns 404
            let r_missing_img = client
                .get(format!(
                    "{}/api/image/cyber-blade.101::1/999.jpg",
                    server_url
                ))
                .send()?;
            ensure!(
                r_missing_img.status() == StatusCode::NOT_FOUND,
                "Expected 404 for non-existent image inside CBZ"
            );

            Ok(
                "Proved RAM hit for 001 + prefetched 002..=006 & miss on 007 with CBZ unlinked"
                    .to_string(),
            )
        },
    );
    monitor.record_phase("5. Image LRU Cache & Prefetch Proof", phase_marker);

    // Phase 6: Zero-Padding Upgrade (001 -> 0001) & Partial Local Availability
    let phase_marker = monitor.mark_phase_start();
    run_case(
        &mut results,
        "Padding & Incremental",
        "Upgrades 3-digit CBZ (001) to 4-digit (0001) when Ch 1000 exists & serves existing immediately",
        || {
            let saga_dir = output_dir.join("Padding Saga");
            fs::create_dir_all(&saga_dir)?;
            let old_cbz = saga_dir.join("Padding Saga - Chapter 001.cbz");
            create_dummy_cbz(&old_cbz)?;

            // Verify that GET /chapters returns both chapters with local status and upgrades padding
            let res = client
                .get(format!(
                    "{}/api/manga/padding-saga.202/chapters",
                    server_url
                ))
                .send()?;
            ensure!(
                res.status() == StatusCode::OK,
                "Expected 200 OK for /chapters, got {}",
                res.status()
            );
            let all_chaps: Vec<ApiChapter> = res.json()?;
            ensure!(
                all_chaps.len() == 2,
                "Expected 2 total remote chapters, got {}",
                all_chaps.len()
            );

            let downloaded_chaps: Vec<ApiChapter> = client
                .get(format!(
                    "{}/api/manga/padding-saga.202/chapters?filter=downloaded",
                    server_url
                ))
                .send()?
                .json()?;
            ensure!(
                downloaded_chaps.len() == 1
                    && downloaded_chaps[0].number == "1"
                    && downloaded_chaps[0].downloaded,
                "Expected 1 downloaded chapter [Ch 1]"
            );

            let missing_chaps: Vec<ApiChapter> = client
                .get(format!(
                    "{}/api/manga/padding-saga.202/chapters?filter=missing",
                    server_url
                ))
                .send()?
                .json()?;
            ensure!(
                missing_chaps.len() == 1
                    && missing_chaps[0].number == "1000"
                    && !missing_chaps[0].downloaded,
                "Expected 1 missing chapter [Ch 1000]"
            );

            // Verify that scan_manga_chapters upgraded `Chapter 001.cbz` -> `Chapter 0001.cbz`
            let upgraded_cbz = saga_dir.join("Padding Saga - Chapter 0001.cbz");
            ensure!(
                upgraded_cbz.exists(),
                "Expected Chapter 001.cbz to be upgraded to Chapter 0001.cbz"
            );
            ensure!(
                !old_cbz.exists(),
                "Old 3-digit Chapter 001.cbz should no longer exist"
            );

            // Test selective chapter download: already downloaded Ch 1 returns queued_count=0
            let res_ch1 = client
                .post(format!(
                    "{}/api/manga/padding-saga.202/download",
                    server_url
                ))
                .json(&serde_json::json!({ "chapters": ["1"] }))
                .send()?;
            ensure!(res_ch1.status() == StatusCode::ACCEPTED);
            let ch1_json: serde_json::Value = res_ch1.json()?;
            ensure!(
                ch1_json["queued_count"] == 0,
                "Expected queued_count=0 for already downloaded Ch 1"
            );

            // Trigger selective download of missing chapter via range (Chapter 1000)
            let res_dl = client
                .post(format!(
                    "{}/api/manga/padding-saga.202/download",
                    server_url
                ))
                .json(&serde_json::json!({ "range": "1000" }))
                .send()?;
            ensure!(
                res_dl.status() == StatusCode::ACCEPTED,
                "Expected 202 Accepted from POST /download"
            );
            let dl_json: serde_json::Value = res_dl.json()?;
            ensure!(
                dl_json["queued_count"] == 1,
                "Expected 1 missing chapter queued, got {:?}",
                dl_json["queued_count"]
            );

            // Wait for background download of Chapter 1000 to finish
            let deadline = Instant::now() + Duration::from_secs(10);
            let mut final_chaps: Vec<ApiChapter> = Vec::new();
            while Instant::now() < deadline {
                let list: Vec<ApiChapter> = client
                    .get(format!(
                        "{}/api/manga/padding-saga.202/chapters?filter=downloaded",
                        server_url
                    ))
                    .send()?
                    .json()?;
                if list.len() == 2 {
                    final_chaps = list;
                    break;
                }
                thread::sleep(Duration::from_millis(50));
            }
            ensure!(
                final_chaps.len() == 2,
                "Timed out waiting for Chapter 1000 download"
            );
            ensure!(
                saga_dir.join("Padding Saga - Chapter 1000.cbz").exists(),
                "Missing Chapter 1000.cbz on disk"
            );

            Ok("Upgraded 001.cbz -> 0001.cbz & downloaded missing Ch 1000".to_string())
        },
    );
    monitor.record_phase("6. Padding Upgrade & Partial Sync", phase_marker);

    // Phase 7: SQLite WAL Database Persistence Verification
    let phase_marker = monitor.mark_phase_start();
    run_case(
        &mut results,
        "SQLite Persistence",
        "Verifies library.db WAL mode, local/remote chapter counts, and cron scheduling intervals",
        || {
            let db_path = config_dir.join("library.db");
            ensure!(
                db_path.exists(),
                "library.db not created in config directory"
            );

            let conn = rusqlite::Connection::open(&db_path)?;
            let journal_mode: String = conn.query_row("PRAGMA journal_mode;", [], |r| r.get(0))?;
            ensure!(
                journal_mode.eq_ignore_ascii_case("wal"),
                "Expected WAL journal_mode, got {}",
                journal_mode
            );

            let (title, status, remote, local, interval): (String, String, i64, i64, i64) = conn
                .query_row(
                    "SELECT title, status, remote_chapters, local_chapters, check_interval_hours
                     FROM mangas WHERE id = 'cyber-blade.101'",
                    [],
                    |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?, r.get(4)?)),
                )?;

            ensure!(
                title == "Cyber Blade & <Steel>",
                "DB title mismatch: {}",
                title
            );
            ensure!(status == "Ongoing", "DB status mismatch: {}", status);
            ensure!(
                remote == 3 && local == 3,
                "Expected remote=3, local=3, got {}/{}",
                local,
                remote
            );
            ensure!(
                interval > 0,
                "Expected positive check_interval_hours, got {}",
                interval
            );

            Ok(format!(
                "WAL active; cyber-blade.101 synced ({}/{} ch, interval={}h)",
                local, remote, interval
            ))
        },
    );
    monitor.record_phase("7. SQLite WAL Verification", phase_marker);

    // Phase 8: Headless CLI Mode End-to-End Test
    let phase_marker = monitor.mark_phase_start();
    run_case(
        &mut results,
        "CLI Mode",
        "Runs ifetch CLI in --list and selective chapter download (-c 1,10.5) mode",
        || {
            let cli_out_dir = output_dir.join("_cli_test_out");
            let manga_url = format!("{}/manga/cyber-blade.101", upstream.base_url);

            let list_output = Command::new(ifetch_bin)
                .arg(&manga_url)
                .arg("--list")
                .env("IFETCH_BASE_URL", &upstream.base_url)
                .output()
                .context("Failed to execute ifetch --list")?;
            ensure!(
                list_output.status.success(),
                "ifetch --list exited with error"
            );
            let stdout = String::from_utf8_lossy(&list_output.stdout);
            ensure!(
                stdout.contains("Chapter 10.5: Side Story"),
                "CLI --list output missing Chapter 10.5"
            );

            let dl_output = Command::new(ifetch_bin)
                .arg(&manga_url)
                .arg("-c")
                .arg("1,10.5")
                .arg("-o")
                .arg(&cli_out_dir)
                .arg("-t")
                .arg("2")
                .env("IFETCH_BASE_URL", &upstream.base_url)
                .output()
                .context("Failed to execute ifetch CLI download")?;
            ensure!(dl_output.status.success(), "ifetch CLI download failed");

            let series_dir = cli_out_dir.join("Cyber Blade & Steel");
            ensure!(
                series_dir
                    .join("Cyber Blade & Steel - Chapter 001.cbz")
                    .exists(),
                "CLI did not create Chapter 001.cbz"
            );
            ensure!(
                series_dir
                    .join("Cyber Blade & Steel - Chapter 010.5.cbz")
                    .exists(),
                "CLI did not create Chapter 010.5.cbz"
            );
            ensure!(
                !series_dir
                    .join("Cyber Blade & Steel - Chapter 002.cbz")
                    .exists(),
                "CLI downloaded unselected Chapter 2"
            );

            let _ = fs::remove_dir_all(&cli_out_dir);
            Ok("CLI --list & selective -c 1,10.5 verified".to_string())
        },
    );
    monitor.record_phase("8. Headless CLI Verification", phase_marker);

    results
}

fn run_case<F>(results: &mut Vec<TestCaseResult>, category: &'static str, name: &'static str, f: F)
where
    F: FnOnce() -> Result<String>,
{
    let start = Instant::now();
    match f() {
        Ok(details) => {
            results.push(TestCaseResult {
                category,
                name,
                passed: true,
                duration: start.elapsed(),
                details,
            });
        }
        Err(e) => {
            results.push(TestCaseResult {
                category,
                name,
                passed: false,
                duration: start.elapsed(),
                details: format!("{:#}", e),
            });
        }
    }
}

fn create_dummy_cbz(path: &Path) -> Result<()> {
    let file = File::create(path)?;
    let mut zip = zip::ZipWriter::new(file);
    let opts = SimpleFileOptions::default().compression_method(zip::CompressionMethod::Stored);
    zip.start_file("ComicInfo.xml", opts)?;
    zip.write_all(b"<ComicInfo/>")?;
    zip.start_file("001.jpg", opts)?;
    let (img, _) = crate::mock_upstream::build_synthetic_image("001.jpg");
    zip.write_all(&img)?;
    zip.finish()?;
    Ok(())
}
