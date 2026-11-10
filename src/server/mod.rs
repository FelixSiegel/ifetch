pub mod cache;
pub mod cron;
pub mod downloader;
pub mod helpers;
pub mod pool;
pub mod routes;
pub mod state;

use crate::{
    config::CACHE_TTL_MINS,
    db::{check_version_update, init_db},
    rate_limit::AdaptiveRateLimiter,
    scraper,
    server::{
        cache::ServerCache, cron::run_cron, pool::DownloadPool, routes::handle_route,
        state::AppState,
    },
};
use log::{error, info, warn};
use std::{
    collections::HashMap,
    io::Read,
    panic::{self, catch_unwind},
    path::PathBuf,
    sync::{Arc, Mutex},
    thread::spawn,
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
        active_downloads: Arc::new(Mutex::new(HashMap::new())),
        db,
        download_pool: Arc::new(DownloadPool::new(threads)),
        cache: Arc::new(ServerCache::new(
            64 * 1024 * 1024,
            Duration::from_secs((*CACHE_TTL_MINS).max(1) * 60),
        )),
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
                let mut request = match server.recv() {
                    Ok(req) => req,
                    Err(_) => break,
                };

                let raw_url = request.url();
                let (path, query) = match parse_request_url(raw_url) {
                    Ok(pq) => pq,
                    Err(e) => {
                        warn!("Malformed URL in request '{}': {}", raw_url, e);
                        let _ = request
                            .respond(Response::from_string("Bad Request").with_status_code(400));
                        continue;
                    }
                };

                let method = request.method().clone();
                let mut body = Vec::new();
                if matches!(
                    method,
                    tiny_http::Method::Post | tiny_http::Method::Put | tiny_http::Method::Patch
                ) {
                    const MAX_REQUEST_BODY_SIZE: u64 = 1024 * 1024; // 1 MiB
                    if let Err(e) = request
                        .as_reader()
                        .take(MAX_REQUEST_BODY_SIZE + 1)
                        .read_to_end(&mut body)
                    {
                        warn!("Failed to read request body for {}: {}", path, e);
                        let _ =
                            request.respond(helpers::bad_request("Failed to read request body"));
                        continue;
                    }

                    if body.len() as u64 > MAX_REQUEST_BODY_SIZE {
                        warn!(
                            "Request body for {} exceeded max size limit of {} bytes",
                            path, MAX_REQUEST_BODY_SIZE
                        );
                        let _ = request.respond(helpers::payload_too_large("Payload Too Large"));
                        continue;
                    }
                }

                info!("Received {} {}", method.as_str(), path);

                let response = catch_unwind(panic::AssertUnwindSafe(|| {
                    handle_route(&method, &path, &query, &body, &state)
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

/// Normalizes and parses an incoming raw HTTP request URL into a path and query parameter map.
fn parse_request_url(raw: &str) -> Result<(String, HashMap<String, String>), url::ParseError> {
    let parsed = if raw.starts_with("http://") || raw.starts_with("https://") {
        Url::parse(raw)?
    } else {
        Url::parse(&format!("http://localhost{}", raw))?
    };

    let path = parsed.path().to_string();
    let query = parsed.query_pairs().into_owned().collect();
    Ok((path, query))
}
