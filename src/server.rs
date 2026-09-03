use crate::{
    config::CRON_HOURS,
    core::{self, manga_chapters},
    db::{get_manga_title, get_mangas_to_check, init_db, upsert_manga},
    discord::{NotificationType, send_webhook},
    models::{Chapter, Manga},
    utils::{chapter_filename, find_chapter_cbz, get_folder_name, get_mime_type, truncate_str},
};
use anyhow::Result;
use log::{error, info, warn};
use reqwest::blocking::Client;
use std::{
    collections::HashSet,
    io::Read,
    path::{Path, PathBuf},
    sync::{Arc, Mutex, MutexGuard},
    thread::sleep,
    time::Duration,
};
use tiny_http::{Header, Response, Server};
use url::Url;

fn lock_mutex<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
}

struct DownloadGuard {
    id: String,
    active_downloads: Arc<Mutex<HashSet<String>>>,
}

impl Drop for DownloadGuard {
    fn drop(&mut self) {
        let mut active = lock_mutex(&self.active_downloads);
        active.remove(&self.id);
    }
}

fn is_valid_segment(s: &str) -> bool {
    !s.is_empty() && !s.contains('/') && !s.contains('\\') && !s.contains("..")
}

pub fn run_server(port: u16, output_dir: PathBuf, threads: usize) {
    if let Err(e) = std::fs::create_dir_all(&output_dir) {
        error!(
            "Failed to create output directory {}: {}",
            output_dir.display(),
            e
        );
        return;
    }
    let _ = env_logger::try_init();

    let server = match Server::http(format!("0.0.0.0:{}", port)) {
        Ok(s) => Arc::new(s),
        Err(e) => {
            error!("Failed to bind server to 0.0.0.0:{}: {}", port, e);
            return;
        }
    };
    info!("Server running on http://0.0.0.0:{}", port);

    let client = match core::build_client() {
        Ok(c) => Arc::new(c),
        Err(e) => {
            error!("Failed to build HTTP client: {}", e);
            return;
        }
    };

    let db_path = output_dir.join("library.db");
    let db = match init_db(&db_path) {
        Ok(d) => Arc::new(Mutex::new(d)),
        Err(e) => {
            error!(
                "Failed to initialize database at {}: {}",
                db_path.display(),
                e
            );
            return;
        }
    };

    let output_dir = Arc::new(output_dir);
    let active_downloads = Arc::new(Mutex::new(HashSet::new()));

    let client_cron = Arc::clone(&client);
    let output_cron = Arc::clone(&output_dir);
    let active_cron = Arc::clone(&active_downloads);
    let db_cron = Arc::clone(&db);
    std::thread::spawn(move || {
        run_cron(&client_cron, &output_cron, threads, &active_cron, &db_cron);
    });

    // Bounded pool of worker threads prevents OS thread exhaustion under load
    let worker_count = (threads * 2).clamp(4, 16);
    let mut handles = Vec::with_capacity(worker_count);

    for _ in 0..worker_count {
        let server = Arc::clone(&server);
        let client = Arc::clone(&client);
        let output_dir = Arc::clone(&output_dir);
        let active_downloads = Arc::clone(&active_downloads);
        let db = Arc::clone(&db);

        let handle = std::thread::spawn(move || {
            loop {
                let request = match server.recv() {
                    Ok(req) => req,
                    Err(_) => break,
                };

                let raw_url = request.url();
                let url_str = format!("http://localhost{}", raw_url);
                let parsed_url = match Url::parse(&url_str) {
                    Ok(u) => u,
                    Err(e) => {
                        warn!("Malformed URL in request '{}': {}", raw_url, e);
                        let _ = request
                            .respond(Response::from_string("Bad Request").with_status_code(400));
                        continue;
                    }
                };

                let path = parsed_url.path().to_string();
                let query: std::collections::HashMap<_, _> =
                    parsed_url.query_pairs().into_owned().collect();

                info!("Received {} {}", request.method().as_str(), path);

                let response = handle_route(
                    &path,
                    &query,
                    &client,
                    &output_dir,
                    threads,
                    &active_downloads,
                    &db,
                );

                match response {
                    Ok(resp) => {
                        let _ = request.respond(resp);
                    }
                    Err(e) => {
                        error!("Error handling request {}: {}", path, e);
                        let _ = request.respond(
                            Response::from_string(format!("Error: {}", e)).with_status_code(500),
                        );
                    }
                }
            }
        });
        handles.push(handle);
    }

    for handle in handles {
        let _ = handle.join();
    }
}

fn resolve_manga(
    id: &str,
    client: &Client,
    output_dir: &Path,
    db: &Arc<Mutex<rusqlite::Connection>>,
) -> Result<(String, PathBuf)> {
    let manga_title = {
        let conn = lock_mutex(db);
        get_manga_title(&conn, id).unwrap_or(None)
    };

    if let Some(title) = manga_title {
        let folder = get_folder_name(&title);
        Ok((title, output_dir.join(folder)))
    } else {
        let url = format!("https://mangakatana.com/manga/{}", id);
        let (manga, chapters) = manga_chapters(client, &url)?;
        let folder = get_folder_name(&manga.title);
        let manga_dir = output_dir.join(&folder);
        let _ = upsert_manga(
            &lock_mutex(db),
            id,
            &manga.title,
            &manga.status,
            chapters.len(),
            0,
            false,
        );
        Ok((manga.title, manga_dir))
    }
}

fn spawn_background_download(
    client: &Client,
    id: &str,
    output_dir: &Path,
    threads: usize,
    active_downloads: &Arc<Mutex<HashSet<String>>>,
    db: &Arc<Mutex<rusqlite::Connection>>,
    preloaded: Option<(Manga, Vec<Chapter>)>,
) {
    let mut active = lock_mutex(active_downloads);
    if active.contains(id) {
        info!(
            "Download for {} is already in progress. Skipping duplicate thread.",
            id
        );
        return;
    }
    active.insert(id.to_string());
    drop(active);

    info!(
        "Missing chapters detected for {}. Triggering background download.",
        id
    );

    let client_bg = client.clone();
    let output_bg = output_dir.to_path_buf();
    let active_bg = Arc::clone(active_downloads);
    let db_bg = Arc::clone(db);
    let id_clone = id.to_string();

    std::thread::spawn(move || {
        let _guard = DownloadGuard {
            id: id_clone.clone(),
            active_downloads: active_bg,
        };

        if let Err(e) = download_background(
            &client_bg, &id_clone, &output_bg, threads, &db_bg, preloaded,
        ) {
            error!("Background download for {} failed: {}", id_clone, e);
            send_webhook(
                &client_bg,
                NotificationType::Error {
                    manga_title: &id_clone,
                    manga_url: &format!("https://mangakatana.com/manga/{}", id_clone),
                    error_msg: &e.to_string(),
                },
            );
        }
    });
}

fn handle_route(
    path: &str,
    query: &std::collections::HashMap<String, String>,
    client: &Client,
    output_dir: &Path,
    threads: usize,
    active_downloads: &Arc<Mutex<HashSet<String>>>,
    db: &Arc<Mutex<rusqlite::Connection>>,
) -> Result<Response<std::io::Cursor<Vec<u8>>>> {
    // GET /api/search?q={query}
    if path == "/api/search" {
        if let Some(q) = query.get("q") {
            let results = core::search_manga(client, q)?;
            let json = serde_json::to_string(&results)?;
            return Ok(Response::from_string(json).with_header(
                Header::from_bytes(&b"Content-Type"[..], &b"application/json"[..]).unwrap(),
            ));
        }
        return Ok(Response::from_string("Missing q").with_status_code(400));
    }

    // GET /api/manga/{id}
    if let Some(id) = path
        .strip_prefix("/api/manga/")
        .filter(|&p| !p.contains('/'))
    {
        if !is_valid_segment(id) {
            return Ok(Response::from_string("Invalid manga ID").with_status_code(400));
        }
        let url = format!("https://mangakatana.com/manga/{}", id);
        let (manga, chapters) = manga_chapters(client, &url)?;
        let _ = upsert_manga(
            &lock_mutex(db),
            id,
            &manga.title,
            &manga.status,
            chapters.len(),
            0,
            false,
        );
        let json = serde_json::to_string(&manga)?;
        return Ok(Response::from_string(json).with_header(
            Header::from_bytes(&b"Content-Type"[..], &b"application/json"[..]).unwrap(),
        ));
    }

    // GET /api/manga/{id}/chapters
    if let Some(rest) = path.strip_prefix("/api/manga/")
        && let Some(id) = rest.strip_suffix("/chapters")
    {
        if !is_valid_segment(id) {
            return Ok(Response::from_string("Invalid manga ID").with_status_code(400));
        }
        let url = format!("https://mangakatana.com/manga/{}", id);
        let (manga, chapters) = manga_chapters(client, &url)?;

        let folder = get_folder_name(&manga.title);
        let manga_dir = output_dir.join(&folder);
        let _ = std::fs::create_dir_all(&manga_dir);

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

        let _ = upsert_manga(
            &lock_mutex(db),
            id,
            &manga.title,
            &manga.status,
            chapters.len(),
            existing_chapters.len(),
            false,
        );

        if existing_chapters.len() < chapters.len() {
            spawn_background_download(
                client,
                id,
                output_dir,
                threads,
                active_downloads,
                db,
                Some((manga, chapters)),
            );

            if existing_chapters.is_empty() {
                return Ok(Response::from_string("").with_status_code(202).with_header(
                    Header::from_bytes(&b"Content-Type"[..], &b"text/plain"[..]).unwrap(),
                ));
            }
        }

        let json = serde_json::to_string(&existing_chapters)?;
        return Ok(Response::from_string(json).with_header(
            Header::from_bytes(&b"Content-Type"[..], &b"application/json"[..]).unwrap(),
        ));
    }

    // GET /api/chapter/{id}::{number}/pages
    if let Some(rest) = path.strip_prefix("/api/chapter/")
        && let Some(chap_id) = rest.strip_suffix("/pages")
    {
        let parts: Vec<&str> = chap_id.split("::").collect();
        if parts.len() == 2 {
            let id = parts[0];
            let number = parts[1];

            if !is_valid_segment(id) || !is_valid_segment(number) {
                return Ok(Response::from_string("Invalid chapter ID").with_status_code(400));
            }

            let (title, manga_dir) = resolve_manga(id, client, output_dir, db)?;

            let cbz_path = match find_chapter_cbz(&manga_dir, &title, number) {
                Some(p) => p,
                None => {
                    warn!(
                        "Requested pages for missing chapter: {} / ch {}",
                        id, number
                    );
                    return Ok(Response::from_string("Not downloaded yet").with_status_code(404));
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
            let json = serde_json::to_string(&pages)?;
            return Ok(Response::from_string(json).with_header(
                Header::from_bytes(&b"Content-Type"[..], &b"application/json"[..]).unwrap(),
            ));
        }
    }

    // GET /api/image/{id}::{number}/{filename}
    if let Some(rest) = path.strip_prefix("/api/image/") {
        let parts: Vec<&str> = rest.split('/').collect();
        if parts.len() == 2 {
            let chap_id = parts[0];
            let filename = parts[1];

            if !is_valid_segment(filename) {
                return Ok(Response::from_string("Invalid filename").with_status_code(400));
            }

            let id_parts: Vec<&str> = chap_id.split("::").collect();
            if id_parts.len() == 2 {
                let id = id_parts[0];
                let number = id_parts[1];

                if !is_valid_segment(id) || !is_valid_segment(number) {
                    return Ok(Response::from_string("Invalid chapter ID").with_status_code(400));
                }

                let (title, manga_dir) = resolve_manga(id, client, output_dir, db)?;

                let cbz_path = match find_chapter_cbz(&manga_dir, &title, number) {
                    Some(p) => p,
                    None => {
                        return Ok(Response::from_string("Chapter not found").with_status_code(404));
                    }
                };

                let file = std::fs::File::open(&cbz_path)?;
                let mut archive = zip::ZipArchive::new(file)?;
                let mut img_file = match archive.by_name(filename) {
                    Ok(f) => f,
                    Err(_) => {
                        return Ok(Response::from_string("Image not found in chapter")
                            .with_status_code(404));
                    }
                };

                let mut buf = Vec::with_capacity(img_file.size() as usize);
                img_file.read_to_end(&mut buf)?;

                let ct = get_mime_type(filename);

                return Ok(Response::from_data(buf)
                    .with_header(Header::from_bytes(&b"Content-Type"[..], ct.as_bytes()).unwrap())
                    .with_header(
                        Header::from_bytes(
                            &b"Cache-Control"[..],
                            &b"public, max-age=31536000, immutable"[..],
                        )
                        .unwrap(),
                    ));
            }
        }
    }

    Ok(Response::from_string("Not Found").with_status_code(404))
}

fn download_background(
    client: &Client,
    id: &str,
    output_dir: &Path,
    threads: usize,
    db: &Arc<Mutex<rusqlite::Connection>>,
    preloaded: Option<(Manga, Vec<Chapter>)>,
) -> Result<()> {
    use rayon::prelude::*;

    let (manga, chapters) = match preloaded {
        Some(data) => data,
        None => {
            let url = format!("https://mangakatana.com/manga/{}", id);
            manga_chapters(client, &url)?
        }
    };

    let url = format!("https://mangakatana.com/manga/{}", id);
    let folder_name = get_folder_name(&manga.title);
    let manga_output_dir = output_dir.join(folder_name);
    std::fs::create_dir_all(&manga_output_dir)?;

    let max_width = crate::utils::determine_width(&chapters);
    crate::utils::upgrade_padding(&manga.title, &chapters, &manga_output_dir, max_width);

    // Identify missing chapters to download
    let missing: Vec<&Chapter> = chapters
        .iter()
        .filter(|ch| {
            let filename = chapter_filename(&manga.title, &ch.number.to_string(), max_width);
            !manga_output_dir.join(&filename).exists()
        })
        .collect();

    if missing.is_empty() {
        let _ = upsert_manga(
            &lock_mutex(db),
            id,
            &manga.title,
            &manga.status,
            chapters.len(),
            chapters.len(),
            false,
        );
        info!("All chapters for {} already downloaded.", id);
        return Ok(());
    }

    let desc = truncate_str(&manga.description, 200);

    send_webhook(
        client,
        NotificationType::Start {
            manga_title: &manga.title,
            manga_url: &url,
            description: &desc,
            chapter_count: missing.len(),
        },
    );

    let current_index = std::sync::atomic::AtomicUsize::new(0);
    let missing_len = missing.len();
    let pb = indicatif::ProgressBar::hidden();

    let pool = rayon::ThreadPoolBuilder::new()
        .num_threads(threads)
        .build()?;

    pool.install(|| {
        (0..threads).into_par_iter().for_each(|_| {
            loop {
                let i = current_index.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                if i >= missing_len {
                    break;
                }
                let chapter = missing[i];
                let _ = core::download_chapter(
                    client,
                    &manga,
                    chapter,
                    &manga_output_dir,
                    max_width,
                    false,
                    &pb,
                );
            }
        });
    });

    let mut local_count = 0;
    for chapter in &chapters {
        let filename = chapter_filename(&manga.title, &chapter.number.to_string(), max_width);
        if manga_output_dir.join(&filename).exists() {
            local_count += 1;
        }
    }

    let _ = upsert_manga(
        &lock_mutex(db),
        id,
        &manga.title,
        &manga.status,
        chapters.len(),
        local_count,
        true,
    );

    send_webhook(
        client,
        NotificationType::Success {
            manga_title: &manga.title,
            manga_url: &url,
            chapter_count: missing_len,
        },
    );

    info!(
        "Background download for {} completed ({} / {} total chapters).",
        id,
        local_count,
        chapters.len()
    );
    Ok(())
}

fn run_cron(
    client: &Client,
    output_dir: &Path,
    threads: usize,
    active_downloads: &Arc<Mutex<HashSet<String>>>,
    db: &Arc<Mutex<rusqlite::Connection>>,
) {
    if *CRON_HOURS <= 0 {
        info!("IFETCH_CRON_HOURS is <= 0. Auto-updates disabled.");
        return;
    }

    sleep(Duration::from_secs(60));

    loop {
        info!("Starting periodic auto-update cron job...");
        let mangas_to_check = {
            let conn = lock_mutex(db);
            get_mangas_to_check(&conn).unwrap_or_default()
        };

        for manga_chk in mangas_to_check {
            sleep(Duration::from_secs(2));
            let url = format!("https://mangakatana.com/manga/{}", manga_chk.id);
            if let Ok((manga, chapters)) = manga_chapters(client, &url) {
                let folder = get_folder_name(&manga.title);
                let manga_dir = output_dir.join(&folder);
                let max_width = crate::utils::determine_width(&chapters);
                crate::utils::upgrade_padding(&manga.title, &chapters, &manga_dir, max_width);

                let mut local_count = 0;
                for chapter in &chapters {
                    let filename =
                        chapter_filename(&manga.title, &chapter.number.to_string(), max_width);
                    if manga_dir.join(&filename).exists() {
                        local_count += 1;
                    }
                }

                let did_update = local_count < chapters.len();
                let _ = upsert_manga(
                    &lock_mutex(db),
                    &manga_chk.id,
                    &manga.title,
                    &manga.status,
                    chapters.len(),
                    local_count,
                    did_update,
                );

                if did_update {
                    spawn_background_download(
                        client,
                        &manga_chk.id,
                        output_dir,
                        threads,
                        active_downloads,
                        db,
                        Some((manga, chapters)),
                    );
                }
            }
        }
        sleep(Duration::from_secs(3600));
    }
}
