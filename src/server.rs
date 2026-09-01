use crate::core;
use anyhow::Result;
use log::{error, info, warn};
use reqwest::blocking::Client;
use std::io::Read;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use tiny_http::{Header, Response, Server};
use url::Url;

pub fn run_server(port: u16, output_dir: PathBuf, threads: usize) {
    env_logger::init();
    let server = Server::http(format!("0.0.0.0:{}", port)).unwrap();
    info!("Server running on http://0.0.0.0:{}", port);
    let client = Arc::new(core::build_client().unwrap());
    let output_dir = Arc::new(output_dir);

    for request in server.incoming_requests() {
        let client_clone = Arc::clone(&client);
        let output_clone = Arc::clone(&output_dir);

        std::thread::spawn(move || {
            let url_str = format!("http://localhost{}", request.url());
            let parsed_url = Url::parse(&url_str).unwrap();
            let path = parsed_url.path().to_string();
            let query: std::collections::HashMap<_, _> =
                parsed_url.query_pairs().into_owned().collect();

            info!("Received {} {}", request.method().as_str(), path);

            let response = handle_route(&path, &query, &client_clone, &output_clone, threads);

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
        let (manga, _) = core::manga_chapters(client, &url)?;
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
        let (manga, chapters) = core::manga_chapters(client, &url)?;

        let folder = get_folder_name(&manga.title);
        let manga_dir = output_dir.join(&folder);

        let max_width = chapters
            .iter()
            .map(|c| c.number.to_string().split('.').next().unwrap().len())
            .max()
            .unwrap_or(3)
            .max(3);

        let mut existing_chapters = Vec::new();
        for chapter in &chapters {
            let filename = crate::utils::chapter_filename(
                &manga.title,
                &chapter.number.to_string(),
                max_width,
            );
            let cbz_path = manga_dir.join(&filename);
            if cbz_path.exists() {
                existing_chapters.push(chapter.clone());
            }
        }

        if existing_chapters.len() < chapters.len() {
            // Background download missing ones
            info!(
                "Missing chapters detected for {}. Triggering background download.",
                id
            );
            let client_bg = client.clone();
            let output_bg = output_dir.to_path_buf();
            let id_clone = id.to_string();
            std::thread::spawn(move || {
                let _ = download_background(&client_bg, &id_clone, &output_bg, threads);
            });

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
            let (manga, chapters) = core::manga_chapters(client, &url)?;

            let max_width = chapters
                .iter()
                .map(|c| c.number.to_string().split('.').next().unwrap().len())
                .max()
                .unwrap_or(3)
                .max(3);
            let filename = crate::utils::chapter_filename(&manga.title, number, max_width);
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
                let (manga, chapters) = core::manga_chapters(client, &url)?;

                let max_width = chapters
                    .iter()
                    .map(|c| c.number.to_string().split('.').next().unwrap().len())
                    .max()
                    .unwrap_or(3)
                    .max(3);
                let cbz_filename = crate::utils::chapter_filename(&manga.title, number, max_width);
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

fn download_background(
    client: &Client,
    id: &str,
    output_dir: &Path,
    threads: usize,
) -> Result<()> {
    use rayon::prelude::*;
    let url = format!("https://mangakatana.com/manga/{}", id);
    let (manga, chapters) = core::manga_chapters(client, &url)?;

    let folder_name = get_folder_name(&manga.title);

    let manga_output_dir = output_dir.join(folder_name);
    std::fs::create_dir_all(&manga_output_dir)?;

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
                let _ = crate::core::download_chapter(
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

    info!("Background download for {} completed.", id);
    Ok(())
}
