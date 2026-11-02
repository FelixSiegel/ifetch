pub mod cache;
pub mod cron;
pub mod downloader;
pub mod helpers;
pub mod routes;
pub mod state;

use crate::{
    db::{check_version_update, init_db},
    rate_limit::AdaptiveRateLimiter,
    scraper,
    server::{
        cache::ServerCache, cron::run_cron, downloader::DownloadPool, routes::handle_route,
        state::AppState,
    },
};
use log::{error, info, warn};
use std::{
    collections::{HashMap, HashSet},
    panic::{self, catch_unwind},
    path::PathBuf,
    sync::{Arc, Mutex},
    thread::spawn,
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
    let bound_port = server
        .server_addr()
        .to_ip()
        .map(|a| a.port())
        .unwrap_or(port);
    let server = Arc::new(server);
    info!("Server running on http://0.0.0.0:{}", bound_port);

    let client = Arc::new(scraper::build_client()?);

    let db_path = config_dir.join("library.db");
    let conn = init_db(&db_path).map_err(|e| {
        anyhow::anyhow!(
            "Failed to initialize database at {}: {}",
            db_path.display(),
            e
        )
    })?;

    let version = env!("CARGO_PKG_VERSION");
    if let Ok(Some(old_ver)) = check_version_update(&conn, version) {
        crate::discord::send_webhook(
            &client,
            crate::discord::NotificationType::Update {
                old_version: old_ver.as_deref(),
                new_version: version,
            },
        );
    }

    let db = Arc::new(Mutex::new(conn));

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
