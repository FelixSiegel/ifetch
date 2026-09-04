use crate::server::{cache::ServerCache, downloader::DownloadPool};
use reqwest::blocking::Client;
use std::{
    collections::HashSet,
    path::PathBuf,
    sync::{Arc, Mutex},
};

pub struct AppState {
    pub client: Arc<Client>,
    pub output_dir: Arc<PathBuf>,
    pub active_downloads: Arc<Mutex<HashSet<String>>>,
    pub db: Arc<Mutex<rusqlite::Connection>>,
    pub download_pool: Arc<DownloadPool>,
    pub cache: Arc<ServerCache>,
}
