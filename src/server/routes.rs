use crate::{
    db::{CheckTrigger, get_manga_title, upsert_manga},
    scraper::{latest_manga, manga_chapters, new_manga, popular_manga, search_manga},
    server::{
        downloader::queue_background_download,
        helpers::{
            accepted_empty, bad_request, image_response, is_valid_segment, json_response,
            lock_mutex, not_found, parse_chapter_id,
        },
        state::AppState,
    },
    utils::{find_chapter_cbz, get_folder_name, get_mime_type, scan_manga_chapters},
};
use anyhow::Result;
use log::warn;
use std::{collections::HashMap, io::Cursor, io::Read, path::PathBuf, sync::Arc};
use tiny_http::Response;

/// Dispatches incoming HTTP requests to their respective endpoint handlers.
pub fn handle_route(
    path: &str,
    query: &HashMap<String, String>,
    state: &Arc<AppState>,
) -> Result<Response<Cursor<Vec<u8>>>> {
    let path = path.strip_suffix('/').unwrap_or(path);

    if path.is_empty() || path == "/health" {
        return Ok(Response::from_string("OK").with_header(
            tiny_http::Header::from_bytes(&b"Content-Type"[..], &b"text/plain"[..]).unwrap(),
        ));
    }

    if path == "/api/search" {
        return search(query, state);
    }
    if path == "/api/popular" {
        return popular(query, state);
    }
    if path == "/api/latest" {
        return latest(query, state);
    }
    if path == "/api/new" {
        return new(query, state);
    }
    if let Some(rest) = path.strip_prefix("/api/manga/") {
        if let Some(id) = rest.strip_suffix("/chapters") {
            return manga_chapters_route(id, state);
        }
        if !rest.contains('/') && !rest.is_empty() {
            return manga_details(rest, state);
        }
    }
    if let Some(rest) = path.strip_prefix("/api/chapter/")
        && let Some(chap_id) = rest.strip_suffix("/pages")
    {
        return chapter_pages(chap_id, state);
    }
    if let Some(rest) = path.strip_prefix("/api/image/") {
        return image(rest, state);
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

/// Fetches `(Manga, Vec<Chapter>)` from the short-lived TTL cache or scrapes MangaKatana on a miss.
fn fetch_or_cache_remote_manga(
    id: &str,
    state: &AppState,
) -> Result<(crate::models::Manga, Vec<crate::models::Chapter>)> {
    if let Some(cached) = state.cache.get_remote_manga(id) {
        return Ok(cached);
    }

    let url = format!("{}/manga/{}", *crate::config::BASE_URL, id);
    let (manga, chapters) = manga_chapters(&state.client, &url)?;
    state
        .cache
        .put_remote_manga(id.to_string(), manga.clone(), chapters.clone());
    Ok((manga, chapters))
}

/// GET /api/manga/{id}
/// Returns metadata for the specified manga ID.
fn manga_details(id: &str, state: &AppState) -> Result<Response<Cursor<Vec<u8>>>> {
    if !is_valid_segment(id) {
        return Ok(bad_request("Invalid manga ID"));
    }

    let (manga, chapters) = fetch_or_cache_remote_manga(id, state)?;

    let folder = get_folder_name(&manga.title);
    let manga_dir = state.output_dir.join(&folder);
    lock_mutex(&state.cache.manga_dirs).insert(id.to_string(), (manga.title.clone(), manga_dir));

    if let Err(e) = upsert_manga(
        &lock_mutex(&state.db),
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

    json_response(&manga)
}

/// GET /api/manga/{id}/chapters
/// Returns existing downloaded chapters, and queues a background download if missing chapters exist.
fn manga_chapters_route(id: &str, state: &Arc<AppState>) -> Result<Response<Cursor<Vec<u8>>>> {
    if !is_valid_segment(id) {
        return Ok(bad_request("Invalid manga ID"));
    }

    let (manga, chapters) = fetch_or_cache_remote_manga(id, state)?;

    let folder = get_folder_name(&manga.title);
    let manga_dir = state.output_dir.join(&folder);

    {
        let mut dirs = lock_mutex(&state.cache.manga_dirs);
        dirs.insert(id.to_string(), (manga.title.clone(), manga_dir.clone()));
    }

    let (_width, existing_chapters, missing_chapters) =
        scan_manga_chapters(&manga_dir, &manga.title, &chapters);
    let has_new = !missing_chapters.is_empty();

    let _ = upsert_manga(
        &lock_mutex(&state.db),
        id,
        &manga.title,
        &manga.status,
        chapters.len(),
        Some(existing_chapters.len()),
        CheckTrigger::UserRequest {
            new_chapters: has_new,
        },
    );

    if has_new {
        queue_background_download(id, state, Some((manga, chapters)));

        if existing_chapters.is_empty() {
            return Ok(accepted_empty());
        }
    }

    json_response(&existing_chapters)
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
    if let Some(p) = cached {
        if p.exists() {
            return Ok(Some(p));
        }
    }

    let (title, manga_dir) = resolve_manga(id, state)?;
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
        .filter(|name| *name != "ComicInfo.xml" && !name.ends_with('/'))
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
        .filter(|name| *name != "ComicInfo.xml" && !name.ends_with('/'))
        .map(String::from)
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
fn resolve_manga(id: &str, state: &AppState) -> Result<(String, PathBuf)> {
    if let Some(entry) = lock_mutex(&state.cache.manga_dirs).get(id).cloned() {
        return Ok(entry);
    }

    let maybe_title = {
        let conn = lock_mutex(&state.db);
        get_manga_title(&conn, id).unwrap_or(None)
    };

    let title = if let Some(title) = maybe_title {
        title
    } else {
        let (manga, chapters) = fetch_or_cache_remote_manga(id, state)?;
        let _ = upsert_manga(
            &lock_mutex(&state.db),
            id,
            &manga.title,
            &manga.status,
            chapters.len(),
            None,
            CheckTrigger::UserRequest {
                new_chapters: false,
            },
        );
        manga.title
    };

    let folder = get_folder_name(&title);
    let dir = state.output_dir.join(folder);
    lock_mutex(&state.cache.manga_dirs).insert(id.to_string(), (title.clone(), dir.clone()));
    Ok((title, dir))
}
