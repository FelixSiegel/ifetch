use crate::{
    config::CRON_HOURS,
    core::{self, manga_chapters},
    db::{get_mangas_to_check, init_db, upsert_manga},
    discord::{NotificationType, send_webhook},
    utils::{chapter_filename, truncate_str},
};
use anyhow::Result;
use log::{error, info, warn};
use reqwest::blocking::Client;
use std::{
    collections::HashSet,
    io::Read,
    path::{Path, PathBuf},
    sync::{Arc, Mutex},
    thread::sleep,
    time::Duration,
};
use tiny_http::{Header, Response, Server};
use url::Url;

pub fn run_server(port: u16, output_dir: PathBuf, threads: usize) {
    env_logger::init();
    let server = Server::http(format!("0.0.0.0:{}", port)).unwrap();
    info!("Server running on http://0.0.0.0:{}", port);
    let client = Arc::new(core::build_client().unwrap());
    let output_dir = Arc::new(output_dir);
    let active_downloads = Arc::new(Mutex::new(HashSet::new()));
    let db = Arc::new(Mutex::new(init_db(output_dir.join("library.db")).unwrap()));

    let client_cron = Arc::clone(&client);
    let output_cron = Arc::clone(&output_dir);
    let active_cron = Arc::clone(&active_downloads);
    let db_cron = Arc::clone(&db);
    std::thread::spawn(move || {
        run_cron(&client_cron, &output_cron, threads, &active_cron, &db_cron);
    });

    for request in server.incoming_requests() {
        let client_clone = Arc::clone(&client);
        let output_clone = Arc::clone(&output_dir);
        let active_clone = Arc::clone(&active_downloads);
        let db_clone = Arc::clone(&db);

        std::thread::spawn(move || {
            let url_str = format!("http://localhost{}", request.url());
            let parsed_url = Url::parse(&url_str).unwrap();
            let path = parsed_url.path().to_string();
            let query: std::collections::HashMap<_, _> =
                parsed_url.query_pairs().into_owned().collect();

            info!("Received {} {}", request.method().as_str(), path);

            let response = handle_route(
                &path,
                &query,
                &client_clone,
                &output_clone,
                threads,
                &active_clone,
                &db_clone,
            );

            match response {
                Ok(resp) => {
                    let _ = request.respond(resp);
                }
                Err(e) => {
                    error!("Error handling request: {}", e);
                    let _ = request.respond(
                        Response::from_string(format!("Error: {}", e)).with_status_code(500),
                    );
                }
            }
        });
    }
}

fn get_folder_name(title: &str) -> String {
    let folder_name = title.replace(|c: char| r#"<>:"/\|?*"#.contains(c), "");
    let folder_name = folder_name.trim();
    if folder_name.is_empty() {
        "manga".to_string()
    } else {
        folder_name.to_string()
    }
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
        let url = format!("https://mangakatana.com/manga/{}", id);
        let (manga, _) = manga_chapters(client, &url)?;
        let json = serde_json::to_string(&manga)?;
        return Ok(Response::from_string(json).with_header(
            Header::from_bytes(&b"Content-Type"[..], &b"application/json"[..]).unwrap(),
        ));
    }

    // GET /api/manga/{id}/chapters
    if let Some(rest) = path.strip_prefix("/api/manga/")
        && let Some(id) = rest.strip_suffix("/chapters")
    {
        let url = format!("https://mangakatana.com/manga/{}", id);
        let (manga, chapters) = manga_chapters(client, &url)?;

        let folder = get_folder_name(&manga.title);
        let manga_dir = output_dir.join(&folder);
        let _ = std::fs::create_dir_all(&manga_dir);

        let max_width = chapters
            .iter()
            .map(|c| c.number.to_string().split('.').next().unwrap().len())
            .max()
            .unwrap_or(3)
            .max(3);

        let mut existing_chapters = Vec::new();
        for chapter in &chapters {
            let filename = chapter_filename(&manga.title, &chapter.number.to_string(), max_width);
            let cbz_path = manga_dir.join(&filename);
            if cbz_path.exists() {
                existing_chapters.push(chapter.clone());
            }
        }

        let _ = upsert_manga(
            &db.lock().unwrap(),
            id,
            &manga.title,
            &manga.status,
            chapters.len(),
            existing_chapters.len(),
            false,
        );

        if existing_chapters.len() < chapters.len() {
            let id_clone = id.to_string();
            let mut active = active_downloads.lock().unwrap();
            if active.contains(&id_clone) {
                info!(
                    "Download for {} is already in progress. Skipping duplicate thread.",
                    id
                );
            } else {
                active.insert(id_clone.clone());
                drop(active);
                // Background download missing ones
                info!(
                    "Missing chapters detected for {}. Triggering background download.",
                    id
                );
                let client_bg = client.clone();
                let output_bg = output_dir.to_path_buf();
                let active_bg = active_downloads.clone();
                std::thread::spawn(move || {
                    if let Err(e) = download_background(&client_bg, &id_clone, &output_bg, threads)
                    {
                        send_webhook(
                            &client_bg,
                            NotificationType::Error {
                                manga_title: &id_clone,
                                manga_url: &format!("https://mangakatana.com/manga/{}", id_clone),
                                error_msg: &e.to_string(),
                            },
                        );
                    }
                    active_bg.lock().unwrap().remove(&id_clone);
                });
            }

            if existing_chapters.is_empty() {
                // Nothing downloaded yet, return 202
                return Ok(Response::from_string("").with_status_code(202).with_header(
                    Header::from_bytes(&b"Content-Type"[..], &b"text/plain"[..]).unwrap(),
                ));
            }
        }

        // Return chapter list of only existing chapters
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
            let url = format!("https://mangakatana.com/manga/{}", id);
            let (manga, chapters) = manga_chapters(client, &url)?;

            let max_width = chapters
                .iter()
                .map(|c| c.number.to_string().split('.').next().unwrap().len())
                .max()
                .unwrap_or(3)
                .max(3);
            let filename = chapter_filename(&manga.title, number, max_width);
            let folder = get_folder_name(&manga.title);
            let cbz_path = output_dir.join(&folder).join(&filename);

            if !cbz_path.exists() {
                warn!(
                    "Requested pages for missing chapter: {}",
                    cbz_path.display()
                );
                return Ok(Response::from_string("Not downloaded yet").with_status_code(404));
            }

            let file = std::fs::File::open(&cbz_path)?;
            let mut archive = zip::ZipArchive::new(file)?;
            let mut pages = Vec::new();
            for i in 0..archive.len() {
                let file = archive.by_index(i)?;
                let name = file.name().to_string();
                if name != "ComicInfo.xml" {
                    pages.push(format!("/api/image/{}/{}", chap_id, name));
                }
            }

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

            let id_parts: Vec<&str> = chap_id.split("::").collect();
            if id_parts.len() == 2 {
                let id = id_parts[0];
                let number = id_parts[1];
                let url = format!("https://mangakatana.com/manga/{}", id);
                let (manga, chapters) = manga_chapters(client, &url)?;

                let max_width = chapters
                    .iter()
                    .map(|c| c.number.to_string().split('.').next().unwrap().len())
                    .max()
                    .unwrap_or(3)
                    .max(3);
                let cbz_filename = chapter_filename(&manga.title, number, max_width);
                let folder = get_folder_name(&manga.title);
                let cbz_path = output_dir.join(&folder).join(&cbz_filename);

                let file = std::fs::File::open(&cbz_path)?;
                let mut archive = zip::ZipArchive::new(file)?;
                let mut img_file = archive.by_name(filename)?;

                let mut buf = Vec::new();
                img_file.read_to_end(&mut buf)?;

                let ct = if filename.ends_with(".png") {
                    "image/png"
                } else {
                    "image/jpeg"
                };

                return Ok(Response::from_data(buf).with_header(
                    Header::from_bytes(&b"Content-Type"[..], ct.as_bytes()).unwrap(),
                ));
            }
        }
    }

    Ok(Response::from_string("Not Found").with_status_code(404))
}

fn download_background(client: &Client, id: &str, output_dir: &Path, threads: usize) -> Result<()> {
    use rayon::prelude::*;
    let url = format!("https://mangakatana.com/manga/{}", id);
    let (manga, chapters) = manga_chapters(client, &url)?;

    let folder_name = get_folder_name(&manga.title);

    let manga_output_dir = output_dir.join(folder_name);
    std::fs::create_dir_all(&manga_output_dir)?;

    let desc = truncate_str(&manga.description, 200);

    send_webhook(
        client,
        NotificationType::Start {
            manga_title: &manga.title,
            manga_url: &url,
            description: &desc,
            chapter_count: chapters.len(),
        },
    );

    let max_width = chapters
        .iter()
        .map(|c| c.number.to_string().split('.').next().unwrap().len())
        .max()
        .unwrap_or(3)
        .max(3);

    let current_index = std::sync::atomic::AtomicUsize::new(0);
    let chosen_len = chapters.len();

    let pb = indicatif::ProgressBar::hidden();

    let pool = rayon::ThreadPoolBuilder::new()
        .num_threads(threads)
        .build()?;

    pool.install(|| {
        (0..threads).into_par_iter().for_each(|_| {
            loop {
                let i = current_index.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                if i >= chosen_len {
                    break;
                }
                let chapter = &chapters[i];
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

    send_webhook(
        client,
        NotificationType::Success {
            manga_title: &manga.title,
            manga_url: &url,
            chapter_count: chapters.len(),
        },
    );

    info!("Background download for {} completed.", id);
    Ok(())
}

fn run_cron(
    client: &Client,
    output_dir: &Path,
    threads: usize,
    active_downloads: &Arc<Mutex<HashSet<String>>>,
    db: &Arc<Mutex<rusqlite::Connection>>,
) {
    if *CRON_HOURS == 0 {
        info!("IFETCH_CRON_HOURS is set to 0. Auto-updates disabled.");
        return;
    }

    // Sleep on startup to not block other startup stuff
    sleep(Duration::from_secs(60));

    loop {
        info!("Starting periodic auto-update cron job...");
        let mangas_to_check = { get_mangas_to_check(&db.lock().unwrap()).unwrap_or_default() };

        for manga_chk in mangas_to_check {
            sleep(Duration::from_secs(2));
            let url = format!("https://mangakatana.com/manga/{}", manga_chk.id);
            if let Ok((manga, chapters)) = manga_chapters(client, &url) {
                let folder = get_folder_name(&manga.title);
                let manga_dir = output_dir.join(&folder);
                let max_width = chapters
                    .iter()
                    .map(|c| c.number.to_string().split('.').next().unwrap().len())
                    .max()
                    .unwrap_or(3)
                    .max(3);

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
                    &db.lock().unwrap(),
                    &manga_chk.id,
                    &manga.title,
                    &manga.status,
                    chapters.len(),
                    local_count,
                    did_update,
                );

                if did_update {
                    let mut active = active_downloads.lock().unwrap();
                    if !active.contains(&manga_chk.id) {
                        active.insert(manga_chk.id.clone());
                        drop(active);
                        info!(
                            "Cron detected missing chapters for {}. Triggering background download.",
                            manga_chk.id
                        );
                        let client_bg = client.clone();
                        let output_bg = output_dir.to_path_buf();
                        let active_bg = active_downloads.clone();
                        let id = manga_chk.id.clone();
                        std::thread::spawn(move || {
                            if let Err(e) =
                                download_background(&client_bg, &id, &output_bg, threads)
                            {
                                send_webhook(
                                    &client_bg,
                                    NotificationType::Error {
                                        manga_title: &id,
                                        manga_url: &format!("https://mangakatana.com/manga/{}", id),
                                        error_msg: &e.to_string(),
                                    },
                                );
                            }
                            active_bg.lock().unwrap().remove(&id);
                        });
                    }
                }
            }
        }
        sleep(Duration::from_secs(3600));
    }
}
