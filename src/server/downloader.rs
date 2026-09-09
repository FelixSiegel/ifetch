use crate::{
    core::{self, manga_chapters},
    db::{CheckTrigger, upsert_manga},
    discord::{NotificationType, send_webhook},
    models::{Chapter, Manga},
    server::{helpers::lock_mutex, state::AppState},
    utils::{self, chapter_filename, get_folder_name, truncate_str, upgrade_padding},
};
use log::{error, info, warn};
use std::{
    collections::HashSet,
    panic::{self, catch_unwind},
    path::PathBuf,
    sync::{
        Arc, Mutex,
        atomic::{AtomicUsize, Ordering},
        mpsc::{Sender, channel},
    },
    thread::{Builder, JoinHandle},
};

pub type Job = Box<dyn FnOnce() + Send + 'static>;

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

pub struct DownloadGuard {
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

pub struct MangaDownloadTracker {
    id: String,
    manga: Manga,
    url: String,
    total_chapters: usize,
    missing_count: usize,
    remaining: AtomicUsize,
    success_count: AtomicUsize,
    error_count: AtomicUsize,
    first_error: Mutex<Option<String>>,
    output_dir: PathBuf,
    max_width: usize,
    chapters: Vec<Chapter>,
}

pub fn queue_background_download(
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

        let max_width = utils::determine_width(&chapters);
        upgrade_padding(&manga.title, &chapters, &manga_output_dir, max_width);

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
                CheckTrigger::UserRequest {
                    new_chapters: false,
                },
            );
            info!("All chapters for {} already downloaded.", id);
            return;
        }

        let existing_count = chapters.len().saturating_sub(missing.len());
        let desc = truncate_str(&manga.description, 200);
        send_webhook(
            &state.client,
            NotificationType::Start {
                manga_title: &manga.title,
                manga_url: &url,
                description: &desc,
                total_chapters: chapters.len(),
                existing_chapters: existing_count,
                missing_chapters: missing.len(),
            },
        );

        let missing_len = missing.len();
        let tracker = Arc::new(MangaDownloadTracker {
            id: id.clone(),
            manga: manga.clone(),
            url,
            total_chapters: chapters.len(),
            missing_count: missing_len,
            remaining: AtomicUsize::new(missing_len),
            success_count: AtomicUsize::new(0),
            error_count: AtomicUsize::new(0),
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
                let mut attempts = 0;
                let max_attempts = 3;

                let res = loop {
                    attempts += 1;
                    state.rate_limiter.wait_for_chapter_permit();

                    let pb = indicatif::ProgressBar::hidden();
                    let dl_res = core::download_chapter(
                        &state.client,
                        &tracker.manga,
                        &chapter,
                        &tracker.output_dir,
                        tracker.max_width,
                        false,
                        &pb,
                    );

                    match dl_res {
                        Ok(val) => {
                            state.rate_limiter.on_chapter_success();
                            break Ok(val);
                        }
                        Err(e) if is_rate_limit_error(&e) && attempts < max_attempts => {
                            let cooldown = state.rate_limiter.on_rate_limit_hit(&e.to_string());
                            warn!(
                                "Rate limit encountered on chapter {} for {}. Pausing worker for {:?} before retry (attempt {}/{})",
                                chapter.number, tracker.manga.title, cooldown, attempts, max_attempts
                            );
                            std::thread::sleep(cooldown);
                        }
                        Err(e) => break Err(e),
                    }
                };

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
                        let mut fe = lock_mutex(&tracker.first_error);
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

fn is_rate_limit_error(e: &anyhow::Error) -> bool {
    let msg = e.to_string();
    msg.contains("Rate limited")
        || msg.contains("429")
        || msg.contains("403")
        || msg.contains("503")
        || msg.contains("Cloudflare")
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
        CheckTrigger::DownloadComplete {
            success: did_update,
        },
    );

    let first_err_guard = lock_mutex(&tracker.first_error);
    let first_err = first_err_guard.as_deref();

    if total_errors > 0 {
        warn!(
            "Background download for {} finished with errors ({} succeeded, {} failed out of {} missing). First error: {:?}",
            tracker.id, total_successes, total_errors, tracker.missing_count, first_err
        );
    } else {
        info!(
            "Background download for {} completed successfully ({} / {} total chapters).",
            tracker.id, local_count, tracker.total_chapters
        );
    }

    let stats = state.rate_limiter.get_stats();
    info!(
        "Limiter stats: total dl={}, blocks hit={}, min burst before block={:?}, max burst={}, current burst={}, current delay={}ms, cooldown={}s",
        stats.total_chapters,
        stats.total_blocks_hit,
        stats.min_burst_before_block,
        stats.max_burst_achieved,
        stats.current_burst_count,
        stats.current_delay_ms,
        stats.current_cooldown_secs,
    );

    send_webhook(
        &state.client,
        NotificationType::Complete {
            manga_title: &tracker.manga.title,
            manga_url: &tracker.url,
            total_chapters: tracker.total_chapters,
            downloaded_now: total_successes,
            failed_now: total_errors,
            total_available: local_count,
            first_error: first_err,
        },
    );

    let mut active = lock_mutex(&state.active_downloads);
    active.remove(&tracker.id);
}
