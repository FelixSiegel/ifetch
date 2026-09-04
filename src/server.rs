pub mod cache;
pub mod downloader;
pub mod helpers;
pub mod routes;
pub mod state;

use crate::{
    config::CRON_HOURS,
    core::{self, manga_chapters},
    db::{get_mangas_to_check, init_db, upsert_manga},
    server::{
        cache::ServerCache,
        downloader::{DownloadPool, queue_background_download},
        helpers::lock_mutex,
        routes::handle_route,
        state::AppState,
    },
    utils::{chapter_filename, get_folder_name},
};
use log::{error, info, warn};
use std::{
    collections::{HashMap, HashSet},
    panic::{self, catch_unwind},
    path::PathBuf,
    sync::{Arc, Mutex},
    thread::{sleep, spawn},
    time::Duration,
};
use tiny_http::{Response, Server};
use url::Url;

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

    let state = Arc::new(AppState {
        client,
        output_dir: Arc::new(output_dir),
        active_downloads: Arc::new(Mutex::new(HashSet::new())),
        db,
        download_pool: Arc::new(DownloadPool::new(threads)),
        cache: Arc::new(ServerCache::new(64 * 1024 * 1024)), // 64 MB LRU image cache
    });

    let state_cron = Arc::clone(&state);
    spawn(move || {
        run_cron(&state_cron);
    });

    let worker_count = (threads * 2).clamp(4, 16);
    let mut handles = Vec::with_capacity(worker_count);

    for _ in 0..worker_count {
        let server = Arc::clone(&server);
        let state = Arc::clone(&state);

        let handle = spawn(move || {
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
                let query: HashMap<_, _> = parsed_url.query_pairs().into_owned().collect();

                info!("Received {} {}", request.method().as_str(), path);

                let response = catch_unwind(panic::AssertUnwindSafe(|| {
                    handle_route(&path, &query, &state)
                }));

                match response {
                    Ok(Ok(resp)) => {
                        let _ = request.respond(resp);
                    }
                    Ok(Err(e)) => {
                        error!("Error handling request {}: {}", path, e);
                        let _ = request.respond(
                            Response::from_string(format!("Error: {}", e)).with_status_code(500),
                        );
                    }
                    Err(panic_err) => {
                        error!("Panic while handling request {}: {:?}", path, panic_err);
                        let _ = request.respond(
                            Response::from_string("Internal Server Error").with_status_code(500),
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

fn run_cron(state: &Arc<AppState>) {
    if *CRON_HOURS <= 0 {
        info!("IFETCH_CRON_HOURS is <= 0. Auto-updates disabled.");
        return;
    }

    sleep(Duration::from_secs(60));

    loop {
        let cron_cycle = catch_unwind(panic::AssertUnwindSafe(|| {
            info!("Starting periodic auto-update cron job...");
            let mangas_to_check = {
                let conn = lock_mutex(&state.db);
                get_mangas_to_check(&conn).unwrap_or_default()
            };

            for manga_chk in mangas_to_check {
                sleep(Duration::from_secs(2));
                let url = format!("https://mangakatana.com/manga/{}", manga_chk.id);
                if let Ok((manga, chapters)) = manga_chapters(&state.client, &url) {
                    let folder = get_folder_name(&manga.title);
                    let manga_dir = state.output_dir.join(&folder);
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
                        &lock_mutex(&state.db),
                        &manga_chk.id,
                        &manga.title,
                        &manga.status,
                        chapters.len(),
                        Some(local_count),
                        did_update,
                    );

                    if did_update {
                        queue_background_download(&manga_chk.id, state, Some((manga, chapters)));
                    }
                }
            }
        }));

        if let Err(e) = cron_cycle {
            error!("Cron cycle encountered panic: {:?}", e);
        }

        sleep(Duration::from_secs(3600));
    }
}
