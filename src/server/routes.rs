use crate::{
    db::{CheckTrigger, get_manga_title, manga_exists, upsert_manga},
    models::Chapter,
    scraper::{
        is_not_found_error, latest_manga, manga_chapters, new_manga, popular_manga, search_manga,
    },
    server::{
        downloader::{MangaDownloadBatch, cancel_download, queue_download_chapters},
        helpers::{
            bad_request, image_response, is_valid_segment, json_response,
            json_response_with_status, lock_mutex, method_not_allowed, not_found, parse_chapter_id,
        },
        state::AppState,
    },
    utils::{find_chapter_cbz, get_folder_name, get_mime_type, scan_manga_chapters},
};
use anyhow::Result;
use log::warn;
use rust_decimal::Decimal;
use serde::{Deserialize, Serialize};
use std::{
    collections::{HashMap, HashSet},
    io::{Cursor, Read},
    path::PathBuf,
    sync::Arc,
};
use tiny_http::{Method, Response};

#[derive(Debug, Serialize)]
pub struct MangaDownloadStatus {
    pub manga_id: String,
    pub is_downloading: bool,
    pub queued_count: usize,
    pub completed_count: usize,
    pub error_count: usize,
    pub remaining_count: usize,
}

#[derive(Debug, Serialize)]
pub struct QueueSummary {
    pub active_mangas: usize,
    pub queued_chapters: usize,
}

#[derive(Debug, Serialize)]
pub struct DownloadsTelemetryResponse {
    pub active_downloads: Vec<String>,
    pub active_count: usize,
    pub queued_chapters: usize,
    pub queue_summary: QueueSummary,
    pub downloads: Vec<MangaDownloadStatus>,
}

#[derive(Debug, Serialize)]
struct QueuedDownloadResponse<'a> {
    status: &'static str,
    manga_id: &'a str,
    queued_count: usize,
}

#[derive(Debug, Serialize)]
struct CancelledDownloadResponse<'a> {
    status: &'static str,
    manga_id: &'a str,
}

#[derive(Debug, Deserialize, Default)]
struct DownloadPayload {
    #[serde(default)]
    all: Option<bool>,
    #[serde(default)]
    chapters: Option<Vec<serde_json::Value>>,
    #[serde(default)]
    range: Option<String>,
}

fn require_get<F>(method: &Method, handler: F) -> Result<Response<Cursor<Vec<u8>>>>
where
    F: FnOnce() -> Result<Response<Cursor<Vec<u8>>>>,
{
    if *method == Method::Get {
        handler()
    } else {
        Ok(method_not_allowed("Method Not Allowed"))
    }
}

/// Dispatches incoming HTTP requests to their respective endpoint handlers.
pub fn handle_route(
    method: &Method,
    path: &str,
    query: &HashMap<String, String>,
    body: &[u8],
    state: &Arc<AppState>,
) -> Result<Response<Cursor<Vec<u8>>>> {
    let path = path.strip_suffix('/').unwrap_or(path);

    if path.is_empty() || path == "/health" {
        return require_get(method, || {
            Ok(Response::from_string("OK").with_header(
                tiny_http::Header::from_bytes(&b"Content-Type"[..], &b"text/plain"[..]).unwrap(),
            ))
        });
    }

    match path {
        "/api/search" => return require_get(method, || search(query, state)),
        "/api/popular" => return require_get(method, || popular(query, state)),
        "/api/latest" => return require_get(method, || latest(query, state)),
        "/api/new" => return require_get(method, || new(query, state)),
        "/api/downloads" => return require_get(method, || downloads_telemetry(state)),
        _ => {}
    }

    if let Some(rest) = path.strip_prefix("/api/manga/") {
        if let Some(id) = rest.strip_suffix("/chapters") {
            return require_get(method, || manga_chapters_route(id, query, state));
        }
        if let Some(id) = rest.strip_suffix("/download") {
            return match *method {
                Method::Post => trigger_download(id, body, state),
                Method::Delete => cancel_download_route(id, state),
                Method::Get => manga_download_status_route(id, state),
                _ => Ok(method_not_allowed("Method Not Allowed")),
            };
        }
        if !rest.contains('/') && !rest.is_empty() {
            return require_get(method, || manga_details(rest, state));
        }
    }
    if let Some(rest) = path.strip_prefix("/api/chapter/")
        && let Some(chap_id) = rest.strip_suffix("/pages")
    {
        return require_get(method, || chapter_pages(chap_id, state));
    }
    if let Some(rest) = path.strip_prefix("/api/image/") {
        return require_get(method, || image(rest, state));
    }

    Ok(not_found("Not Found"))
}

/// GET /api/search?q={query}
/// Searches for manga matching the query parameter `q`.
fn search(query: &HashMap<String, String>, state: &AppState) -> Result<Response<Cursor<Vec<u8>>>> {
    let Some(q) = query.get("q") else {
        return Ok(bad_request("Missing q"));
    };

    let results = search_manga(&state.client, q)?;
    json_response(&results)
}

fn parse_page_param(query: &HashMap<String, String>) -> usize {
    query
        .get("page")
        .or_else(|| query.get("p"))
        .and_then(|s| s.parse::<usize>().ok())
        .filter(|&p| p > 0)
        .unwrap_or(1)
}

/// GET /api/popular
/// Returns hot/popular manga from the front page.
fn popular(query: &HashMap<String, String>, state: &AppState) -> Result<Response<Cursor<Vec<u8>>>> {
    let page = parse_page_param(query);
    let results = popular_manga(&state.client, page)?;
    json_response(&results)
}

/// GET /api/latest
/// Returns latest updated manga from MangaKatana.
fn latest(query: &HashMap<String, String>, state: &AppState) -> Result<Response<Cursor<Vec<u8>>>> {
    let page = parse_page_param(query);
    let results = latest_manga(&state.client, page)?;
    json_response(&results)
}

/// GET /api/new
/// Returns newly added manga from MangaKatana.
fn new(query: &HashMap<String, String>, state: &AppState) -> Result<Response<Cursor<Vec<u8>>>> {
    let page = parse_page_param(query);
    let results = new_manga(&state.client, page)?;
    json_response(&results)
}

const PREFETCH_PAGES: usize = 5;

/// Fetches `(Manga, Vec<Chapter>, is_cached)` from the short-lived TTL cache or scrapes MangaKatana on a miss.
fn fetch_or_cache_remote_manga(
    id: &str,
    state: &AppState,
) -> Result<(crate::models::Manga, Vec<Chapter>, bool)> {
    if let Some((manga, chapters)) = state.cache.get_remote_manga(id) {
        return Ok((manga, chapters, true));
    }

    let url = format!("{}/manga/{}", *crate::config::BASE_URL, id);
    let (manga, chapters) = manga_chapters(&state.client, &url)?;
    state
        .cache
        .put_remote_manga(id.to_string(), manga.clone(), chapters.clone());
    Ok((manga, chapters, false))
}

/// Cache manga directory mapping if not already present, and return `(folder_name, manga_dir)`.
fn cache_manga_dir(id: &str, title: &str, state: &AppState) -> PathBuf {
    let folder = get_folder_name(title);
    let manga_dir = state.output_dir.join(folder);
    lock_mutex(&state.cache.manga_dirs)
        .entry(id.to_string())
        .or_insert_with(|| (title.to_string(), manga_dir.clone()));
    manga_dir
}

/// GET /api/manga/{id}
/// Returns metadata for the specified manga ID.
fn manga_details(id: &str, state: &AppState) -> Result<Response<Cursor<Vec<u8>>>> {
    if !is_valid_segment(id) {
        return Ok(bad_request("Invalid manga ID"));
    }

    let (manga, chapters, is_cached) = match fetch_or_cache_remote_manga(id, state) {
        Ok(res) => res,
        Err(e) if is_not_found_error(&e) => return Ok(not_found("Manga not found")),
        Err(e) => return Err(e),
    };
    let _ = cache_manga_dir(id, &manga.title, state);

    if !is_cached {
        let conn = lock_mutex(&state.db);
        if manga_exists(&conn, id).unwrap_or(false) {
            if let Err(e) = upsert_manga(
                &conn,
                id,
                &manga.title,
                &manga.status,
                chapters.len(),
                None,
                CheckTrigger::UserRequest {
                    new_chapters: false,
                },
            ) {
                warn!("Failed to upsert manga {} in database: {}", id, e);
            }
        }
    }

    json_response(&manga)
}

/// GET /api/manga/{id}/chapters
/// Returns remote chapters marked with local download status, without triggering downloads.
fn manga_chapters_route(
    id: &str,
    query: &HashMap<String, String>,
    state: &Arc<AppState>,
) -> Result<Response<Cursor<Vec<u8>>>> {
    if !is_valid_segment(id) {
        return Ok(bad_request("Invalid manga ID"));
    }

    let filter_mode = query.get("filter").map(|s| s.as_str()).unwrap_or("all");
    if !matches!(filter_mode, "all" | "downloaded" | "missing") {
        return Ok(bad_request("Invalid filter parameter"));
    }

    let (manga, mut chapters, is_cached) = match fetch_or_cache_remote_manga(id, state) {
        Ok(res) => res,
        Err(e) if is_not_found_error(&e) => return Ok(not_found("Manga not found")),
        Err(e) => return Err(e),
    };
    let manga_dir = cache_manga_dir(id, &manga.title, state);

    let (_width, existing_count) = scan_manga_chapters(&manga_dir, &manga.title, &mut chapters);
    let has_new = existing_count < chapters.len();

    if !is_cached || existing_count > 0 {
        let conn = lock_mutex(&state.db);
        let in_db = manga_exists(&conn, id).unwrap_or(false);
        if (!is_cached && in_db) || (!in_db && existing_count > 0) {
            let _ = upsert_manga(
                &conn,
                id,
                &manga.title,
                &manga.status,
                chapters.len(),
                Some(existing_count),
                CheckTrigger::UserRequest {
                    new_chapters: has_new,
                },
            );
        }
    }

    match filter_mode {
        "downloaded" => chapters.retain(|c| c.downloaded),
        "missing" => chapters.retain(|c| !c.downloaded),
        _ => {}
    }

    json_response(&chapters)
}

/// POST /api/manga/{id}/download
/// Triggers explicit download of chapters for a manga.
fn trigger_download(
    id: &str,
    body: &[u8],
    state: &Arc<AppState>,
) -> Result<Response<Cursor<Vec<u8>>>> {
    if !is_valid_segment(id) {
        return Ok(bad_request("Invalid manga ID"));
    }

    let payload: DownloadPayload = if body.iter().all(|b| b.is_ascii_whitespace()) {
        DownloadPayload {
            all: Some(true),
            chapters: None,
            range: None,
        }
    } else {
        match serde_json::from_slice::<DownloadPayload>(body) {
            Ok(p) => p,
            Err(e) => return Ok(bad_request(&format!("Invalid JSON body: {}", e))),
        }
    };

    if lock_mutex(&state.active_downloads).contains_key(id) {
        return json_response_with_status(
            202,
            &QueuedDownloadResponse {
                status: "queued",
                manga_id: id,
                queued_count: 0,
            },
        );
    }

    let (manga, mut chapters, _) = match fetch_or_cache_remote_manga(id, state) {
        Ok(res) => res,
        Err(e) if is_not_found_error(&e) => return Ok(not_found("Manga not found")),
        Err(e) => return Err(e),
    };
    let manga_dir = cache_manga_dir(id, &manga.title, state);

    let (max_width, existing_count) = scan_manga_chapters(&manga_dir, &manga.title, &mut chapters);

    let to_download: Vec<Chapter> = if let Some(chap_vals) = payload.chapters {
        if chap_vals.is_empty() {
            return Ok(bad_request("Requested chapters not found in manga"));
        }
        let mut target_numbers = HashSet::with_capacity(chap_vals.len());
        for val in chap_vals {
            let dec = match val {
                serde_json::Value::String(s) => Decimal::from_str_exact(s.trim())
                    .map_err(|_| format!("Invalid chapter number: {}", s)),
                serde_json::Value::Number(n) => {
                    let s = n.to_string();
                    Decimal::from_str_exact(&s)
                        .map_err(|_| format!("Invalid chapter number: {}", s))
                }
                _ => return Ok(bad_request("Chapter elements must be numbers or strings")),
            };
            match dec {
                Ok(d) => {
                    target_numbers.insert(d);
                }
                Err(msg) => return Ok(bad_request(&msg)),
            }
        }

        let mut matched_any = false;
        let to_dl: Vec<Chapter> = chapters
            .iter()
            .filter(|c| {
                if target_numbers.contains(&c.number) {
                    matched_any = true;
                    !c.downloaded
                } else {
                    false
                }
            })
            .cloned()
            .collect();

        if !matched_any {
            return Ok(bad_request("Requested chapters not found in manga"));
        }
        to_dl
    } else if let Some(range_str) = payload.range {
        let matched = match crate::cli::selector::select_chapters(&chapters, &range_str) {
            Ok(chaps) => chaps,
            Err(e) => return Ok(bad_request(&format!("Invalid range: {}", e))),
        };

        matched.into_iter().filter(|c| !c.downloaded).collect()
    } else if payload.all.unwrap_or(true) {
        chapters.iter().filter(|c| !c.downloaded).cloned().collect()
    } else {
        Vec::new()
    };

    let queued_count = queue_download_chapters(
        id,
        state,
        MangaDownloadBatch {
            manga,
            chapters,
            to_download,
            existing_count,
            max_width,
            output_dir: manga_dir,
        },
    );

    json_response_with_status(
        202,
        &QueuedDownloadResponse {
            status: "queued",
            manga_id: id,
            queued_count,
        },
    )
}

/// DELETE /api/manga/{id}/download
/// Cancels any queued/active downloads for the given manga ID.
fn cancel_download_route(id: &str, state: &AppState) -> Result<Response<Cursor<Vec<u8>>>> {
    if !is_valid_segment(id) {
        return Ok(bad_request("Invalid manga ID"));
    }

    let _ = cancel_download(id, state);
    json_response(&CancelledDownloadResponse {
        status: "cancelled",
        manga_id: id,
    })
}

/// GET /api/manga/{id}/download
/// Returns status for this specific manga.
fn manga_download_status_route(id: &str, state: &AppState) -> Result<Response<Cursor<Vec<u8>>>> {
    if !is_valid_segment(id) {
        return Ok(bad_request("Invalid manga ID"));
    }

    let status = lock_mutex(&state.active_downloads)
        .get(id)
        .map(|tracker| tracker.to_status())
        .unwrap_or_else(|| MangaDownloadStatus {
            manga_id: id.to_string(),
            is_downloading: false,
            queued_count: 0,
            completed_count: 0,
            error_count: 0,
            remaining_count: 0,
        });

    json_response(&status)
}

/// GET /api/downloads
/// Returns active download IDs and queue summary.
fn downloads_telemetry(state: &AppState) -> Result<Response<Cursor<Vec<u8>>>> {
    let mut downloads: Vec<MangaDownloadStatus> = lock_mutex(&state.active_downloads)
        .values()
        .map(|tracker| tracker.to_status())
        .collect();

    downloads.sort_by(|a, b| a.manga_id.cmp(&b.manga_id));

    let total_queued: usize = downloads.iter().map(|d| d.remaining_count).sum();
    let active_downloads: Vec<String> = downloads.iter().map(|d| d.manga_id.clone()).collect();
    let active_count = downloads.len();

    json_response(&DownloadsTelemetryResponse {
        active_downloads,
        active_count,
        queued_chapters: total_queued,
        queue_summary: QueueSummary {
            active_mangas: active_count,
            queued_chapters: total_queued,
        },
        downloads,
    })
}

/// Resolves and caches the `.cbz` archive path for a chapter.
fn resolve_chapter_cbz(
    chap_id: &str,
    id: &str,
    number: &str,
    state: &AppState,
) -> Result<Option<PathBuf>> {
    let cached = {
        let cbz_map = lock_mutex(&state.cache.chapter_cbz_paths);
        cbz_map.get(chap_id).cloned()
    };
    if let Some(p) = cached.filter(|p| p.exists()) {
        return Ok(Some(p));
    }

    let Some((title, manga_dir)) = resolve_manga(id, state)? else {
        return Ok(None);
    };
    let Some(cbz_path) = find_chapter_cbz(&manga_dir, &title, number) else {
        return Ok(None);
    };

    {
        let mut cbz_map = lock_mutex(&state.cache.chapter_cbz_paths);
        cbz_map.insert(chap_id.to_string(), cbz_path.clone());
    }

    Ok(Some(cbz_path))
}

/// GET /api/chapter/{id}::{number}/pages
/// Returns the list of image URLs contained within the chapter's CBZ archive.
fn chapter_pages(chap_id: &str, state: &AppState) -> Result<Response<Cursor<Vec<u8>>>> {
    let Some((id, number)) = parse_chapter_id(chap_id) else {
        return Ok(bad_request("Invalid chapter ID"));
    };

    if let Some(pages) = lock_mutex(&state.cache.chapter_pages).get(chap_id).cloned() {
        return json_response(&pages);
    }

    let cbz_path = match resolve_chapter_cbz(chap_id, id, number, state)? {
        Some(p) => p,
        None => {
            warn!(
                "Requested pages for missing chapter: {} / ch {}",
                id, number
            );
            return Ok(not_found("Not downloaded yet"));
        }
    };

    let file = std::fs::File::open(&cbz_path)?;
    let archive = zip::ZipArchive::new(file)?;
    let mut pages: Vec<String> = archive
        .file_names()
        .filter_map(Result::ok)
        .filter(|name| name != "ComicInfo.xml" && !name.ends_with('/'))
        .map(|name| format!("/api/image/{}/{}", chap_id, name))
        .collect();

    pages.sort();

    lock_mutex(&state.cache.chapter_pages).insert(chap_id.to_string(), pages.clone());

    json_response(&pages)
}

/// GET /api/image/{id}::{number}/{filename}
/// Serves an individual image extracted from a chapter's CBZ archive.
fn image(rest: &str, state: &AppState) -> Result<Response<Cursor<Vec<u8>>>> {
    let Some((chap_id, filename)) = rest.split_once('/') else {
        return Ok(not_found("Not Found"));
    };

    if !is_valid_segment(filename) {
        return Ok(bad_request("Invalid filename"));
    }

    let Some((id, number)) = parse_chapter_id(chap_id) else {
        return Ok(bad_request("Invalid chapter ID"));
    };

    let cache_key = format!("{}/{}", chap_id, filename);
    if let Some(cached_data) = lock_mutex(&state.cache.image_cache).get(&cache_key) {
        let ct = get_mime_type(filename);
        return Ok(image_response(&cached_data, ct));
    }

    let cbz_path = match resolve_chapter_cbz(chap_id, id, number, state)? {
        Some(p) => p,
        None => {
            return Ok(not_found("Chapter not found"));
        }
    };

    let file = std::fs::File::open(&cbz_path)?;
    let mut archive = zip::ZipArchive::new(file)?;

    // Extract the requested page first and insert it into the cache immediately
    let requested_data: Arc<[u8]> = {
        let mut img_file = match archive.by_name(filename) {
            Ok(f) => f,
            Err(_) => {
                return Ok(not_found("Image not found in chapter"));
            }
        };

        let max_img_size = img_file.size().min(50 * 1024 * 1024) as usize;
        let mut buf = Vec::with_capacity(max_img_size);
        img_file.read_to_end(&mut buf)?;
        Arc::from(buf.into_boxed_slice())
    };

    lock_mutex(&state.cache.image_cache).insert(cache_key, Arc::clone(&requested_data));

    // Determine the next few page filenames in sorted order and pre-load any that aren't cached yet
    let mut all_names: Vec<String> = archive
        .file_names()
        .filter_map(Result::ok)
        .filter(|name| name != "ComicInfo.xml" && !name.ends_with('/'))
        .map(|s| s.into_owned())
        .collect();
    all_names.sort();

    if let Some(pos) = all_names.iter().position(|n| n == filename) {
        let next_names = &all_names[pos + 1..(pos + 1 + PREFETCH_PAGES).min(all_names.len())];
        let to_load: Vec<(String, String)> = {
            let img_cache = lock_mutex(&state.cache.image_cache);
            next_names
                .iter()
                .filter_map(|name| {
                    let key = format!("{}/{}", chap_id, name);
                    (!img_cache.contains(&key)).then_some((name.clone(), key))
                })
                .collect()
        };

        let mut prefetched = Vec::with_capacity(to_load.len());
        for (next_name, key) in to_load {
            if let Ok(mut entry) = archive.by_name(&next_name) {
                let size = entry.size().min(50 * 1024 * 1024) as usize;
                let mut buf = Vec::with_capacity(size);
                if entry.read_to_end(&mut buf).is_ok() {
                    prefetched.push((key, Arc::<[u8]>::from(buf.into_boxed_slice())));
                }
            }
        }

        if !prefetched.is_empty() {
            let mut img_cache = lock_mutex(&state.cache.image_cache);
            for (key, data) in prefetched {
                img_cache.insert(key, data);
            }
        }
    }

    let ct = get_mime_type(filename);
    Ok(image_response(&requested_data, ct))
}

/// Resolves a manga ID to its title and output directory, consulting the cache and database.
fn resolve_manga(id: &str, state: &AppState) -> Result<Option<(String, PathBuf)>> {
    if let Some(entry) = lock_mutex(&state.cache.manga_dirs).get(id).cloned() {
        return Ok(Some(entry));
    }

    let maybe_title = {
        let conn = lock_mutex(&state.db);
        get_manga_title(&conn, id).unwrap_or(None)
    };

    let title = if let Some(title) = maybe_title {
        title
    } else {
        let (manga, _chapters, _) = match fetch_or_cache_remote_manga(id, state) {
            Ok(res) => res,
            Err(e) if is_not_found_error(&e) => return Ok(None),
            Err(e) => return Err(e),
        };
        manga.title
    };

    let dir = cache_manga_dir(id, &title, state);
    Ok(Some((title, dir)))
}
