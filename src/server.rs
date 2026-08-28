use crate::core;
use anyhow::Result;
use log::{error, info, warn};
use reqwest::blocking::Client;
use std::io::Read;
use std::path::PathBuf;
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
    output_dir: &PathBuf,
    threads: usize,
) -> Result<Response<std::io::Cursor<Vec<u8>>>> {
    if path == "/index.min.json" || path == "/index.min.json/index.min.json" {
        let json = r#"[{"name":"iFetch API","pkg":"eu.kanade.tachiyomi.extension.en.ifetch","apk":"ifetch-v4.apk","lang":"en","code":1,"version":"1.0","nsfw":0,"hasReadme":0,"hasChangelog":0,"sources":[{"id":"2265008544838634865","name":"iFetch API","lang":"en","baseUrl":"http://192.168.2.18:8080"}]}]"#;
        return Ok(Response::from_string(json).with_header(Header::from_bytes(&b"Content-Type"[..], &b"application/json"[..]).unwrap()));
    }
    
    if path.ends_with("repo.json") {
        let json = r#"{"meta":{"name":"iFetch Repo","shortName":"iFetch","website":"http://192.168.2.18:8080","signingKeyFingerprint":"fd61f54a581cfac9d565e68a5db2e7edd84f0044b33a5a384e34d22f88e32293"}}"#;
        return Ok(Response::from_string(json).with_header(Header::from_bytes(&b"Content-Type"[..], &b"application/json"[..]).unwrap()));
    }

    if path.ends_with("icon.png") || path.contains("/icon/") {
        let transparent_png = vec![
            137, 80, 78, 71, 13, 10, 26, 10, 0, 0, 0, 13, 73, 72, 68, 82, 0, 0, 0, 1, 0, 0, 0, 1, 8, 6, 0, 0, 0, 31, 21, 196, 137, 0, 0, 0, 10, 73, 68, 65, 84, 120, 156, 99, 0, 1, 0, 0, 5, 0, 1, 13, 10, 45, 180, 0, 0, 0, 0, 73, 69, 78, 68, 174, 66, 96, 130,
        ];
        return Ok(Response::from_data(transparent_png).with_header(Header::from_bytes(&b"Content-Type"[..], &b"image/png"[..]).unwrap()));
    }

    if path.ends_with(".apk") {
        if let Ok(mut file) = std::fs::File::open("ifetch.apk") {
            let mut buf = Vec::new();
            use std::io::Read;
            if file.read_to_end(&mut buf).is_ok() {
                return Ok(Response::from_data(buf).with_header(Header::from_bytes(&b"Content-Type"[..], &b"application/vnd.android.package-archive"[..]).unwrap()));
            }
        }
        return Ok(Response::from_string("APK not found").with_status_code(404));
    }

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
        .and_then(|p| if p.contains('/') { None } else { Some(p) })
    {
        let url = format!("https://mangakatana.com/manga/{}", id);
        let (manga, _) = core::manga_chapters(client, &url)?;
        let json = serde_json::to_string(&manga)?;
        return Ok(Response::from_string(json).with_header(
            Header::from_bytes(&b"Content-Type"[..], &b"application/json"[..]).unwrap(),
        ));
    }

    // GET /api/manga/{id}/chapters
    if let Some(rest) = path.strip_prefix("/api/manga/") {
        if let Some(id) = rest.strip_suffix("/chapters") {
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
                let output_bg = output_dir.clone();
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
    }

    // GET /api/chapter/{id}::{number}/pages
    if let Some(rest) = path.strip_prefix("/api/chapter/") {
        if let Some(chap_id) = rest.strip_suffix("/pages") {
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
    output_dir: &PathBuf,
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
