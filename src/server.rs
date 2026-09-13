pub mod cache;
pub mod downloader;
pub mod helpers;
pub mod routes;
pub mod state;

use crate::{
    config::CRON_HOURS,
    core::{self, manga_chapters},
    db::{CheckTrigger, get_mangas_to_check, init_db, upsert_manga},
    rate_limit::AdaptiveRateLimiter,
    server::{
        cache::ServerCache,
        downloader::{DownloadPool, queue_background_download},
        helpers::lock_mutex,
        routes::handle_route,
        state::AppState,
    },
    utils::get_folder_name,
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

/// Starts the Tachiyomi/Mihon compatible HTTP server, initializes the background download pool,
/// cache, database, and scheduled cron auto-updater.
pub fn run_server(
    port: u16,
    output_dir: PathBuf,
    config_dir: PathBuf,
    threads: usize,
) -> anyhow::Result<()> {
    std::fs::create_dir_all(&output_dir)?;
    std::fs::create_dir_all(&config_dir)?;

    let server = Server::http(format!("0.0.0.0:{}", port))
        .map_err(|e| anyhow::anyhow!("Failed to bind server to 0.0.0.0:{}: {}", port, e))?;
    let server = Arc::new(server);
    info!("Server running on http://0.0.0.0:{}", port);

    let client = Arc::new(core::build_client()?);

    let db_path = config_dir.join("library.db");
    let db = Arc::new(Mutex::new(init_db(&db_path).map_err(|e| {
        anyhow::anyhow!(
            "Failed to initialize database at {}: {}",
            db_path.display(),
            e
        )
    })?));

    let state = Arc::new(AppState {
        client,
        output_dir: Arc::new(output_dir),
        active_downloads: Arc::new(Mutex::new(HashSet::new())),
        db,
        download_pool: Arc::new(DownloadPool::new(threads)),
        cache: Arc::new(ServerCache::new(64 * 1024 * 1024)), // 64 MB LRU image cache
        rate_limiter: Arc::new(AdaptiveRateLimiter::new()),
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
                let parsed_url =
                    if raw_url.starts_with("http://") || raw_url.starts_with("https://") {
                        Url::parse(raw_url)
                    } else {
                        Url::parse(&format!("http://localhost{}", raw_url))
                    };
                let parsed_url = match parsed_url {
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
                        let is_rate_limit = crate::rate_limit::is_rate_limit_error(&e);
                        let status_code = if is_rate_limit {
                            let retry_after = crate::rate_limit::extract_retry_after(&e);
                            state
                                .rate_limiter
                                .on_rate_limit_hit_with_retry(&e.to_string(), retry_after);
                            429
                        } else {
                            500
                        };
                        let _ = request.respond(
                            Response::from_string(format!("Error: {}", e))
                                .with_status_code(status_code),
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

    Ok(())
}

/// Periodic background task that checks for new chapters of stored mangas and queues downloads.
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
                match manga_chapters(&state.client, &url) {
                    Ok((manga, chapters)) => {
                        let folder = get_folder_name(&manga.title);
                        let manga_dir = state.output_dir.join(&folder);
                        let (_, existing, missing) =
                            crate::utils::scan_manga_chapters(&manga_dir, &manga.title, &chapters);
                        let local_count = existing.len();
                        let did_update = !missing.is_empty();
                        let _ = upsert_manga(
                            &lock_mutex(&state.db),
                            &manga_chk.id,
                            &manga.title,
                            &manga.status,
                            chapters.len(),
                            Some(local_count),
                            CheckTrigger::Cron {
                                new_chapters: did_update,
                            },
                        );

                        if did_update {
                            queue_background_download(
                                &manga_chk.id,
                                state,
                                Some((manga, chapters)),
                            );
                        }
                    }
                    Err(e) => {
                        warn!("Auto-update check failed for {}: {}", manga_chk.id, e);
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
