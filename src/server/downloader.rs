use crate::{
    archive::{ChapterDownloadContext, download_chapter_with_retry},
    db::{CheckTrigger, upsert_manga},
    discord::{NotificationType, send_webhook},
    models::{Chapter, Manga},
    scraper::manga_chapters,
    server::{helpers::lock_mutex, state::AppState},
    utils::{get_folder_name, scan_manga_chapters, truncate_str},
};
use log::{error, info, warn};
use std::{
    collections::HashSet,
    path::PathBuf,
    sync::{
        Arc, Mutex,
        atomic::{AtomicUsize, Ordering},
    },
};

/// RAII guard ensuring the manga ID is removed from `active_downloads` if early exit occurs.
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

/// Tracks asynchronous progress and completion across individual chapter download tasks.
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

impl MangaDownloadTracker {
    /// Creates a new tracker initialized with the missing chapter count.
    pub fn new(
        id: String,
        manga: Manga,
        url: String,
        chapters: Vec<Chapter>,
        output_dir: PathBuf,
        max_width: usize,
        missing_count: usize,
    ) -> Self {
        Self {
            id,
            total_chapters: chapters.len(),
            manga,
            url,
            missing_count,
            remaining: AtomicUsize::new(missing_count),
            success_count: AtomicUsize::new(0),
            error_count: AtomicUsize::new(0),
            first_error: Mutex::new(None),
            output_dir,
            max_width,
            chapters,
        }
    }

    /// Records a successfully downloaded chapter. Returns `true` if this was the last chapter.
    pub fn record_success(&self) -> bool {
        self.success_count.fetch_add(1, Ordering::Relaxed);
        self.remaining.fetch_sub(1, Ordering::SeqCst) == 1
    }

    /// Records a failed chapter download. Returns `true` if this was the last chapter.
    pub fn record_error(&self, error: Option<String>) -> bool {
        self.error_count.fetch_add(1, Ordering::Relaxed);
        if let Some(err) = error {
            let mut fe = lock_mutex(&self.first_error);
            if fe.is_none() {
                *fe = Some(err);
            }
        }
        self.remaining.fetch_sub(1, Ordering::SeqCst) == 1
    }
}

/// RAII guard ensuring chapter completion and error accounting even if a worker panics.
struct ChapterGuard<'a> {
    tracker: &'a MangaDownloadTracker,
    state: &'a AppState,
    completed: bool,
}

impl<'a> Drop for ChapterGuard<'a> {
    fn drop(&mut self) {
        if !self.completed && self.tracker.record_error(None) {
            on_manga_download_complete(self.tracker, self.state);
        }
    }
}

/// Initiates or queues a background download of all missing chapters for the given manga ID.
///
/// Prevents concurrent duplicate downloads for the same manga ID.
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

        let Some((manga, chapters)) = fetch_manga_data(&id, &state, preloaded) else {
            return;
        };

        let url = format!("{}/manga/{}", *crate::config::BASE_URL, id);
        let folder_name = get_folder_name(&manga.title);
        let manga_output_dir = state.output_dir.join(&folder_name);

        lock_mutex(&state.cache.manga_dirs)
            .insert(id.clone(), (manga.title.clone(), manga_output_dir.clone()));

        let (max_width, existing, missing) =
            scan_manga_chapters(&manga_output_dir, &manga.title, &chapters);

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

        let desc = truncate_str(&manga.description, 200);
        send_webhook(
            &state.client,
            NotificationType::Start {
                manga_title: &manga.title,
                manga_url: &url,
                description: &desc,
                total_chapters: chapters.len(),
                existing_chapters: existing.len(),
                missing_chapters: missing.len(),
            },
        );

        let missing_len = missing.len();
        let tracker = Arc::new(MangaDownloadTracker::new(
            id.clone(),
            manga.clone(),
            url,
            chapters.clone(),
            manga_output_dir,
            max_width,
            missing_len,
        ));

        // Disarm guard: tracker completion logic handles removing from active_downloads
        guard.disarmed = true;

        for chapter in missing {
            let tracker = Arc::clone(&tracker);
            let pool = Arc::clone(&state.download_pool);
            let state = Arc::clone(&state);

            pool.spawn(move || {
                let mut guard = ChapterGuard {
                    tracker: &tracker,
                    state: &state,
                    completed: false,
                };

                let pb = indicatif::ProgressBar::hidden();
                let ctx = ChapterDownloadContext {
                    client: &state.client,
                    manga: &tracker.manga,
                    chapter: &chapter,
                    output_dir: &tracker.output_dir,
                    width: tracker.max_width,
                    verify: false,
                };

                let res = download_chapter_with_retry(
                    &ctx,
                    &pb,
                    &state.rate_limiter,
                    |cooldown, attempt, max_attempts| {
                        warn!(
                            "Rate limit encountered on chapter {} for {}. Pausing worker for {:?} before retry (attempt {}/{})",
                            chapter.number, tracker.manga.title, cooldown, attempt, max_attempts
                        );
                    },
                );

                let is_last = match res {
                    Ok(Some(_)) | Ok(None) => tracker.record_success(),
                    Err(e) => {
                        error!(
                            "Failed to download chapter {} for {}: {}",
                            chapter.number, tracker.manga.title, e
                        );
                        tracker.record_error(Some(e.to_string()))
                    }
                };

                guard.completed = true;
                if is_last {
                    on_manga_download_complete(&tracker, &state);
                }
            });
        }
    });
}

/// Retrieves manga details and chapters from preloaded data or fetches them via HTTP.
fn fetch_manga_data(
    id: &str,
    state: &AppState,
    preloaded: Option<(Manga, Vec<Chapter>)>,
) -> Option<(Manga, Vec<Chapter>)> {
    if let Some(data) = preloaded {
        return Some(data);
    }

    let url = format!("{}/manga/{}", *crate::config::BASE_URL, id);
    match manga_chapters(&state.client, &url) {
        Ok(data) => Some(data),
        Err(e) => {
            error!("Failed to fetch chapter list for {}: {}", id, e);
            send_webhook(
                &state.client,
                NotificationType::Error {
                    manga_title: id,
                    manga_url: &url,
                    error_msg: &e.to_string(),
                },
            );
            None
        }
    }
}

/// Finalizer invoked when the last chapter task for a manga finishes downloading.
fn on_manga_download_complete(tracker: &MangaDownloadTracker, state: &AppState) {
    let _guard = DownloadGuard {
        id: tracker.id.clone(),
        active_downloads: Arc::clone(&state.active_downloads),
        disarmed: false,
    };

    let prefix = format!("{}::", tracker.id);
    lock_mutex(&state.cache.chapter_pages).retain(|k, _| !k.starts_with(&prefix));
    lock_mutex(&state.cache.chapter_cbz_paths).retain(|k, _| !k.starts_with(&prefix));

    let total_successes = tracker.success_count.load(Ordering::Relaxed);
    let total_errors = tracker.error_count.load(Ordering::Relaxed);

    let (_, existing, _) =
        scan_manga_chapters(&tracker.output_dir, &tracker.manga.title, &tracker.chapters);
    let local_count = existing.len();

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
        "Limiter stats: total dl={}, blocks hit={}, min burst={:?}, min total={:?}, max burst={}, permits since reset={} (burst: {}), current delay={}ms, cooldown={}s",
        stats.total_successful_chapters,
        stats.total_blocks_hit,
        stats.min_burst_before_block,
        stats.min_total_before_block,
        stats.max_burst_achieved,
        stats.total_permits_since_reset,
        stats.burst_permits_since_reset,
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
}
