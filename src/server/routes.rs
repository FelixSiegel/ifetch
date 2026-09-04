use crate::{
    core::{self, manga_chapters},
    db::{CheckTrigger, get_manga_title, upsert_manga},
    server::{
        downloader::queue_background_download,
        helpers::{
            accepted_empty, bad_request, image_response, is_valid_segment, json_response,
            lock_mutex, not_found, parse_chapter_id,
        },
        state::AppState,
    },
    utils::{chapter_filename, find_chapter_cbz, get_folder_name, get_mime_type},
};
use anyhow::Result;
use log::warn;
use std::{collections::HashMap, io::Cursor, io::Read, path::PathBuf, sync::Arc};
use tiny_http::Response;

pub fn handle_route(
    path: &str,
    query: &HashMap<String, String>,
    state: &Arc<AppState>,
) -> Result<Response<Cursor<Vec<u8>>>> {
    if path == "/api/search" {
        return search(query, state);
    }
    if let Some(id) = path
        .strip_prefix("/api/manga/")
        .filter(|&p| !p.contains('/'))
    {
        return manga_details(id, state);
    }
    if let Some(rest) = path.strip_prefix("/api/manga/")
        && let Some(id) = rest.strip_suffix("/chapters")
    {
        return manga_chapters_route(id, state);
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

// GET /api/search?q={query}
fn search(query: &HashMap<String, String>, state: &AppState) -> Result<Response<Cursor<Vec<u8>>>> {
    let Some(q) = query.get("q") else {
        return Ok(bad_request("Missing q"));
    };

    let results = core::search_manga(&state.client, q)?;
    json_response(&results)
}

// GET /api/manga/{id}
fn manga_details(id: &str, state: &AppState) -> Result<Response<Cursor<Vec<u8>>>> {
    if !is_valid_segment(id) {
        return Ok(bad_request("Invalid manga ID"));
    }

    let url = format!("https://mangakatana.com/manga/{}", id);
    let (manga, chapters) = manga_chapters(&state.client, &url)?;
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

    json_response(&manga)
}

// GET /api/manga/{id}/chapters
fn manga_chapters_route(id: &str, state: &Arc<AppState>) -> Result<Response<Cursor<Vec<u8>>>> {
    if !is_valid_segment(id) {
        return Ok(bad_request("Invalid manga ID"));
    }

    let url = format!("https://mangakatana.com/manga/{}", id);
    let (manga, chapters) = manga_chapters(&state.client, &url)?;

    let folder = get_folder_name(&manga.title);
    let manga_dir = state.output_dir.join(&folder);
    let _ = std::fs::create_dir_all(&manga_dir);

    if let Ok(mut dirs) = state.cache.manga_dirs.lock() {
        dirs.insert(id.to_string(), (manga.title.clone(), manga_dir.clone()));
    }

    let max_width = crate::utils::determine_width(&chapters);
    crate::utils::upgrade_padding(&manga.title, &chapters, &manga_dir, max_width);

    let mut existing_chapters = Vec::new();
    for chapter in &chapters {
        let filename = chapter_filename(&manga.title, &chapter.number.to_string(), max_width);
        let cbz_path = manga_dir.join(&filename);
        if cbz_path.exists() {
            existing_chapters.push(chapter.clone());
        }
    }

    let has_new = existing_chapters.len() < chapters.len();
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

// GET /api/chapter/{id}::{number}/pages
fn chapter_pages(chap_id: &str, state: &AppState) -> Result<Response<Cursor<Vec<u8>>>> {
    let Some((id, number)) = parse_chapter_id(chap_id) else {
        return Ok(bad_request("Invalid chapter ID"));
    };

    if let Ok(pages_map) = state.cache.chapter_pages.lock()
        && let Some(pages) = pages_map.get(chap_id).cloned()
    {
        return json_response(&pages);
    }

    let (title, manga_dir) = resolve_manga(id, state)?;

    let cbz_path = match find_chapter_cbz(&manga_dir, &title, number) {
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

    if let Ok(mut pages_map) = state.cache.chapter_pages.lock() {
        pages_map.insert(chap_id.to_string(), pages.clone());
    }

    json_response(&pages)
}

// GET /api/image/{id}::{number}/{filename}
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
    if let Ok(mut img_cache) = state.cache.image_cache.lock()
        && let Some(cached_data) = img_cache.get(&cache_key)
    {
        let ct = get_mime_type(filename);
        return Ok(image_response(&cached_data, ct));
    }

    let (title, manga_dir) = resolve_manga(id, state)?;

    let cbz_path = match find_chapter_cbz(&manga_dir, &title, number) {
        Some(p) => p,
        None => {
            return Ok(not_found("Chapter not found"));
        }
    };

    let file = std::fs::File::open(&cbz_path)?;
    let mut archive = zip::ZipArchive::new(file)?;
    let mut img_file = match archive.by_name(filename) {
        Ok(f) => f,
        Err(_) => {
            return Ok(not_found("Image not found in chapter"));
        }
    };

    let max_img_size = img_file.size().min(50 * 1024 * 1024) as usize;
    let mut buf = Vec::with_capacity(max_img_size);
    img_file.read_to_end(&mut buf)?;

    let arc_data: Arc<[u8]> = Arc::from(buf.into_boxed_slice());
    if let Ok(mut img_cache) = state.cache.image_cache.lock() {
        img_cache.insert(cache_key, Arc::clone(&arc_data));
    }

    let ct = get_mime_type(filename);
    Ok(image_response(&arc_data, ct))
}

fn resolve_manga(id: &str, state: &AppState) -> Result<(String, PathBuf)> {
    if let Ok(dirs) = state.cache.manga_dirs.lock()
        && let Some(entry) = dirs.get(id).cloned()
    {
        return Ok(entry);
    }

    let manga_title = {
        let conn = lock_mutex(&state.db);
        get_manga_title(&conn, id).unwrap_or(None)
    };

    if let Some(title) = manga_title {
        let folder = get_folder_name(&title);
        let dir = state.output_dir.join(folder);
        if let Ok(mut dirs) = state.cache.manga_dirs.lock() {
            dirs.insert(id.to_string(), (title.clone(), dir.clone()));
        }
        Ok((title, dir))
    } else {
        let url = format!("https://mangakatana.com/manga/{}", id);
        let (manga, chapters) = manga_chapters(&state.client, &url)?;
        let folder = get_folder_name(&manga.title);
        let manga_dir = state.output_dir.join(&folder);
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
        if let Ok(mut dirs) = state.cache.manga_dirs.lock() {
            dirs.insert(id.to_string(), (manga.title.clone(), manga_dir.clone()));
        }
        Ok((manga.title, manga_dir))
    }
}
