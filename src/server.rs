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
    collections::{HashMap, HashSet, VecDeque},
    io::Read,
    panic::{self, catch_unwind},
    path::PathBuf,
    sync::{
        self, Arc, Mutex, MutexGuard,
        atomic::Ordering,
        mpsc::{Sender, channel},
    },
    thread::{Builder, JoinHandle, sleep, spawn},
    time::Duration,
};
use tiny_http::{Header, Response, Server};
use url::Url;

fn lock_mutex<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
}

type Job = Box<dyn FnOnce() + Send + 'static>;

pub struct DownloadPool {
    sender: Sender<Job>,
    _workers: Vec<JoinHandle<()>>,
}

impl DownloadPool {
    pub fn new(threads: usize) -> Self {
        let (sender, receiver) = channel::<Job>();
        let receiver = Arc::new(Mutex::new(receiver));
        let mut workers = Vec::with_capacity(threads);

        for id in 0..threads {
            let rx = Arc::clone(&receiver);
            let handle = Builder::new()
                .name(format!("ifetch-dl-{}", id))
                .spawn(move || {
                    loop {
                        let job = {
                            let rx_guard = match rx.lock() {
                                Ok(g) => g,
                                Err(poisoned) => poisoned.into_inner(),
                            };
                            match rx_guard.recv() {
                                Ok(job) => job,
                                Err(_) => break, // Pool shutting down
                            }
                        };

                        if let Err(panic_err) = catch_unwind(panic::AssertUnwindSafe(job)) {
                            error!("Download worker {} caught panic: {:?}", id, panic_err);
                        }
                    }
                })
                .expect("Failed to spawn download worker thread");
            workers.push(handle);
        }

        Self {
            sender,
            _workers: workers,
        }
    }

    pub fn spawn<F>(&self, f: F)
    where
        F: FnOnce() + Send + 'static,
    {
        if let Err(e) = self.sender.send(Box::new(f)) {
            error!("Failed to queue download task: {}", e);
        }
    }
}

struct ImageLruCache {
    max_bytes: usize,
    current_bytes: usize,
    entries: HashMap<String, Arc<[u8]>>,
    order: VecDeque<(String, usize)>,
}

impl ImageLruCache {
    fn new(max_bytes: usize) -> Self {
        Self {
            max_bytes,
            current_bytes: 0,
            entries: HashMap::new(),
            order: VecDeque::new(),
        }
    }

    fn get(&self, key: &str) -> Option<Arc<[u8]>> {
        self.entries.get(key).cloned()
    }

    fn insert(&mut self, key: String, data: Arc<[u8]>) {
        let size = data.len();
        if size > self.max_bytes {
            return;
        }

        if let Some(existing) = self.entries.remove(&key) {
            self.current_bytes = self.current_bytes.saturating_sub(existing.len());
            self.order.retain(|(k, _)| k != &key);
        }

        while self.current_bytes + size > self.max_bytes {
            if let Some((old_key, old_size)) = self.order.pop_front() {
                self.entries.remove(&old_key);
                self.current_bytes = self.current_bytes.saturating_sub(old_size);
            } else {
                break;
            }
        }

        self.current_bytes += size;
        self.entries.insert(key.clone(), data);
        self.order.push_back((key, size));
    }
}

struct ServerCache {
    manga_dirs: Mutex<HashMap<String, (String, PathBuf)>>,
    chapter_pages: Mutex<HashMap<String, Vec<String>>>,
    image_cache: Mutex<ImageLruCache>,
}

impl ServerCache {
    fn new(max_image_bytes: usize) -> Self {
        Self {
            manga_dirs: Mutex::new(HashMap::new()),
            chapter_pages: Mutex::new(HashMap::new()),
            image_cache: Mutex::new(ImageLruCache::new(max_image_bytes)),
        }
    }
}

struct DownloadGuard {
    id: String,
    active_downloads: Arc<Mutex<HashSet<String>>>,
    disarmed: bool,
}

impl Drop for DownloadGuard {
    fn drop(&mut self) {
        if !self.disarmed {
            let mut active = lock_mutex(&self.active_downloads);
            active.remove(&self.id);
        }
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

struct AppState {
    client: Arc<Client>,
    output_dir: Arc<PathBuf>,
    active_downloads: Arc<Mutex<HashSet<String>>>,
    db: Arc<Mutex<rusqlite::Connection>>,
    download_pool: Arc<DownloadPool>,
    cache: Arc<ServerCache>,
}

fn resolve_manga(id: &str, state: &AppState) -> Result<(String, PathBuf)> {
    if let Ok(dirs) = state.cache.manga_dirs.lock()
        && let Some(entry) = dirs.get(id).cloned()
    {
        return Ok(entry);
    }

    let manga_title = {
        let conn = lock_mutex(&state.db);
        get_manga_title(&conn, id).unwrap_or(None)
    };

    if let Some(title) = manga_title {
        let folder = get_folder_name(&title);
        let dir = state.output_dir.join(folder);
        if let Ok(mut dirs) = state.cache.manga_dirs.lock() {
            dirs.insert(id.to_string(), (title.clone(), dir.clone()));
        }
        Ok((title, dir))
    } else {
        let url = format!("https://mangakatana.com/manga/{}", id);
        let (manga, chapters) = manga_chapters(&state.client, &url)?;
        let folder = get_folder_name(&manga.title);
        let manga_dir = state.output_dir.join(&folder);
        let _ = upsert_manga(
            &lock_mutex(&state.db),
            id,
            &manga.title,
            &manga.status,
            chapters.len(),
            None,
            false,
        );
        if let Ok(mut dirs) = state.cache.manga_dirs.lock() {
            dirs.insert(id.to_string(), (manga.title.clone(), manga_dir.clone()));
        }
        Ok((manga.title, manga_dir))
    }
}

struct MangaDownloadTracker {
    id: String,
    manga: Manga,
    url: String,
    total_chapters: usize,
    missing_count: usize,
    remaining: sync::atomic::AtomicUsize,
    success_count: sync::atomic::AtomicUsize,
    error_count: sync::atomic::AtomicUsize,
    first_error: Mutex<Option<String>>,
    output_dir: PathBuf,
    max_width: usize,
    chapters: Vec<Chapter>,
}

fn on_manga_download_complete(tracker: &MangaDownloadTracker, state: &AppState) {
    if let Ok(mut pages) = state.cache.chapter_pages.lock() {
        pages.retain(|k, _| !k.starts_with(&format!("{}::", tracker.id)));
    }

    let total_successes = tracker.success_count.load(Ordering::Relaxed);
    let total_errors = tracker.error_count.load(Ordering::Relaxed);

    let mut local_count = 0;
    for chapter in &tracker.chapters {
        let filename = chapter_filename(
            &tracker.manga.title,
            &chapter.number.to_string(),
            tracker.max_width,
        );
        if tracker.output_dir.join(&filename).exists() {
            local_count += 1;
        }
    }

    let did_update = total_successes > 0;
    let _ = upsert_manga(
        &lock_mutex(&state.db),
        &tracker.id,
        &tracker.manga.title,
        &tracker.manga.status,
        tracker.total_chapters,
        Some(local_count),
        did_update,
    );

    if total_errors > 0 && total_successes == 0 {
        let err_msg = tracker
            .first_error
            .lock()
            .unwrap()
            .clone()
            .unwrap_or_else(|| "All chapter downloads failed".to_string());
        error!("Background download for {} failed: {}", tracker.id, err_msg);
        send_webhook(
            &state.client,
            NotificationType::Error {
                manga_title: &tracker.manga.title,
                manga_url: &tracker.url,
                error_msg: &err_msg,
            },
        );
    } else {
        if total_errors > 0 {
            warn!(
                "Background download for {} partially succeeded ({} succeeded, {} failed out of {}).",
                tracker.id, total_successes, total_errors, tracker.missing_count
            );
        }
        send_webhook(
            &state.client,
            NotificationType::Success {
                manga_title: &tracker.manga.title,
                manga_url: &tracker.url,
                chapter_count: total_successes,
            },
        );
        info!(
            "Background download for {} completed ({} / {} total chapters).",
            tracker.id, local_count, tracker.total_chapters
        );
    }

    let mut active = lock_mutex(&state.active_downloads);
    active.remove(&tracker.id);
}

fn queue_background_download(
    id: &str,
    state: &Arc<AppState>,
    preloaded: Option<(Manga, Vec<Chapter>)>,
) {
    let mut active = lock_mutex(&state.active_downloads);
    if active.contains(id) {
        info!(
            "Download for {} is already queued or in progress. Skipping duplicate.",
            id
        );
        return;
    }
    active.insert(id.to_string());
    drop(active);

    info!(
        "Missing chapters detected for {}. Enqueuing background download.",
        id
    );

    let id = id.to_string();
    let pool = Arc::clone(&state.download_pool);
    let state = Arc::clone(state);

    pool.spawn(move || {
        let mut guard = DownloadGuard {
            id: id.clone(),
            active_downloads: Arc::clone(&state.active_downloads),
            disarmed: false,
        };

        let (manga, chapters) = match preloaded {
            Some(data) => data,
            None => {
                let url = format!("https://mangakatana.com/manga/{}", id);
                match manga_chapters(&state.client, &url) {
                    Ok(data) => data,
                    Err(e) => {
                        error!("Failed to fetch chapter list for {}: {}", id, e);
                        send_webhook(
                            &state.client,
                            NotificationType::Error {
                                manga_title: &id,
                                manga_url: &url,
                                error_msg: &e.to_string(),
                            },
                        );
                        return;
                    }
                }
            }
        };

        let url = format!("https://mangakatana.com/manga/{}", id);
        let folder_name = get_folder_name(&manga.title);
        let manga_output_dir = state.output_dir.join(&folder_name);
        if let Err(e) = std::fs::create_dir_all(&manga_output_dir) {
            error!(
                "Failed to create directory {}: {}",
                manga_output_dir.display(),
                e
            );
            return;
        }

        if let Ok(mut dirs) = state.cache.manga_dirs.lock() {
            dirs.insert(id.clone(), (manga.title.clone(), manga_output_dir.clone()));
        }

        let max_width = crate::utils::determine_width(&chapters);
        crate::utils::upgrade_padding(&manga.title, &chapters, &manga_output_dir, max_width);

        let missing: Vec<Chapter> = chapters
            .iter()
            .filter(|ch| {
                let filename = chapter_filename(&manga.title, &ch.number.to_string(), max_width);
                !manga_output_dir.join(&filename).exists()
            })
            .cloned()
            .collect();

        if missing.is_empty() {
            let _ = upsert_manga(
                &lock_mutex(&state.db),
                &id,
                &manga.title,
                &manga.status,
                chapters.len(),
                Some(chapters.len()),
                false,
            );
            info!("All chapters for {} already downloaded.", id);
            return;
        }

        let desc = truncate_str(&manga.description, 200);
        send_webhook(
            &state.client,
            NotificationType::Start {
                manga_title: &manga.title,
                manga_url: &url,
                description: &desc,
                chapter_count: missing.len(),
            },
        );

        let missing_len = missing.len();
        let tracker = Arc::new(MangaDownloadTracker {
            id: id.clone(),
            manga: manga.clone(),
            url,
            total_chapters: chapters.len(),
            missing_count: missing_len,
            remaining: sync::atomic::AtomicUsize::new(missing_len),
            success_count: sync::atomic::AtomicUsize::new(0),
            error_count: sync::atomic::AtomicUsize::new(0),
            first_error: Mutex::new(None),
            output_dir: manga_output_dir,
            max_width,
            chapters: chapters.clone(),
        });

        // Disarm guard: tracker completion logic handles removing from active_downloads
        guard.disarmed = true;

        for chapter in missing {
            let tracker = Arc::clone(&tracker);
            let pool = Arc::clone(&state.download_pool);
            let state = Arc::clone(&state);

            pool.spawn(move || {
                let pb = indicatif::ProgressBar::hidden();
                let res = core::download_chapter(
                    &state.client,
                    &tracker.manga,
                    &chapter,
                    &tracker.output_dir,
                    tracker.max_width,
                    false,
                    &pb,
                );

                match res {
                    Ok(Some(_)) | Ok(None) => {
                        tracker.success_count.fetch_add(1, Ordering::Relaxed);
                    }
                    Err(e) => {
                        error!(
                            "Failed to download chapter {} for {}: {}",
                            chapter.number, tracker.manga.title, e
                        );
                        tracker.error_count.fetch_add(1, Ordering::Relaxed);
                        let mut fe = tracker.first_error.lock().unwrap();
                        if fe.is_none() {
                            *fe = Some(e.to_string());
                        }
                    }
                }

                if tracker.remaining.fetch_sub(1, Ordering::SeqCst) == 1 {
                    on_manga_download_complete(&tracker, &state);
                }
            });
        }
    });
}

fn handle_route(
    path: &str,
    query: &HashMap<String, String>,
    state: &Arc<AppState>,
) -> Result<Response<std::io::Cursor<Vec<u8>>>> {
    // GET /api/search?q={query}
    if path == "/api/search" {
        if let Some(q) = query.get("q") {
            let results = core::search_manga(&state.client, q)?;
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
        let (manga, chapters) = manga_chapters(&state.client, &url)?;
        let _ = upsert_manga(
            &lock_mutex(&state.db),
            id,
            &manga.title,
            &manga.status,
            chapters.len(),
            None,
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
        let (manga, chapters) = manga_chapters(&state.client, &url)?;

        let folder = get_folder_name(&manga.title);
        let manga_dir = state.output_dir.join(&folder);
        let _ = std::fs::create_dir_all(&manga_dir);

        if let Ok(mut dirs) = state.cache.manga_dirs.lock() {
            dirs.insert(id.to_string(), (manga.title.clone(), manga_dir.clone()));
        }

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
            &lock_mutex(&state.db),
            id,
            &manga.title,
            &manga.status,
            chapters.len(),
            Some(existing_chapters.len()),
            false,
        );

        if existing_chapters.len() < chapters.len() {
            queue_background_download(id, state, Some((manga, chapters)));

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

            if let Ok(pages_map) = state.cache.chapter_pages.lock()
                && let Some(pages) = pages_map.get(chap_id).cloned()
            {
                let json = serde_json::to_string(&pages)?;
                return Ok(Response::from_string(json).with_header(
                    Header::from_bytes(&b"Content-Type"[..], &b"application/json"[..]).unwrap(),
                ));
            }

            let (title, manga_dir) = resolve_manga(id, state)?;

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

            if let Ok(mut pages_map) = state.cache.chapter_pages.lock() {
                pages_map.insert(chap_id.to_string(), pages.clone());
            }

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

                let cache_key = format!("{}/{}", chap_id, filename);
                if let Ok(img_cache) = state.cache.image_cache.lock()
                    && let Some(cached_data) = img_cache.get(&cache_key)
                {
                    let ct = get_mime_type(filename);
                    return Ok(Response::from_data(cached_data.to_vec())
                        .with_header(
                            Header::from_bytes(&b"Content-Type"[..], ct.as_bytes()).unwrap(),
                        )
                        .with_header(
                            Header::from_bytes(
                                &b"Cache-Control"[..],
                                &b"public, max-age=31536000, immutable"[..],
                            )
                            .unwrap(),
                        ));
                }

                let (title, manga_dir) = resolve_manga(id, state)?;

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

                let max_img_size = img_file.size().min(50 * 1024 * 1024) as usize;
                let mut buf = Vec::with_capacity(max_img_size);
                img_file.read_to_end(&mut buf)?;

                let arc_data: Arc<[u8]> = Arc::from(buf.into_boxed_slice());
                if let Ok(mut img_cache) = state.cache.image_cache.lock() {
                    img_cache.insert(cache_key, Arc::clone(&arc_data));
                }

                let ct = get_mime_type(filename);

                return Ok(Response::from_data(arc_data.to_vec())
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
