use crate::{
    rate_limit::AdaptiveRateLimiter,
    server::{cache::ServerCache, downloader::DownloadPool},
};
use reqwest::blocking::Client;
use std::{
    collections::HashSet,
    path::PathBuf,
    sync::{Arc, Mutex},
};

/// Shared application state accessible across all server routes and background workers.
pub struct AppState {
    /// HTTP client configured with desktop browser User-Agent and connection timeouts.
    pub client: Arc<Client>,
    /// Root directory where manga CBZ archives and covers are stored.
    pub output_dir: Arc<PathBuf>,
    /// Set of manga IDs currently being downloaded in the background to prevent duplicate downloads.
    pub active_downloads: Arc<Mutex<HashSet<String>>>,
    /// SQLite database connection for metadata, intervals, and local/remote chapter tracking.
    pub db: Arc<Mutex<rusqlite::Connection>>,
    /// Fixed-worker thread pool executing queued chapter downloads.
    pub download_pool: Arc<DownloadPool>,
    /// In-memory LRU and metadata caches for chapter pages and images.
    pub cache: Arc<ServerCache>,
    /// Global adaptive rate limiter pacing chapter requests against Cloudflare.
    pub rate_limiter: Arc<AdaptiveRateLimiter>,
}
