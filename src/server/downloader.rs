use crate::{
    archive::{ChapterDownloadContext, download_chapter_with_retry},
    db::{CheckTrigger, upsert_manga},
    discord::{NotificationType, send_webhook},
    models::{Chapter, Manga},
    scraper::manga_chapters,
    server::{helpers::lock_mutex, routes::MangaDownloadStatus, state::AppState},
    utils::{get_folder_name, scan_manga_chapters, truncate_str},
};
use log::{error, info, warn};
use std::{
    collections::HashMap,
    path::PathBuf,
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, AtomicUsize, Ordering},
    },
};

/// RAII guard ensuring the manga ID is removed from `active_downloads` if early exit or panic occurs.
pub struct DownloadGuard {
    id: String,
    active_downloads: Arc<Mutex<HashMap<String, Arc<MangaDownloadTracker>>>>,
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
    pub id: String,
    pub manga: Manga,
    pub url: String,
    pub total_chapters: usize,
    pub missing_count: usize,
    pub remaining: AtomicUsize,
    pub success_count: AtomicUsize,
    pub error_count: AtomicUsize,
    pub first_error: Mutex<Option<String>>,
    pub output_dir: PathBuf,
    pub max_width: usize,
    pub chapters: Vec<Chapter>,
    pub cancelled: AtomicBool,
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
            cancelled: AtomicBool::new(false),
        }
    }

    /// Returns a status snapshot for this manga download.
    pub fn to_status(&self) -> MangaDownloadStatus {
        MangaDownloadStatus {
            manga_id: self.id.clone(),
            is_downloading: true,
            queued_count: self.missing_count,
            completed_count: self.success_count.load(Ordering::Relaxed),
            error_count: self.error_count.load(Ordering::Relaxed),
            remaining_count: self.remaining.load(Ordering::Relaxed),
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
        if !self.completed
            && !self.tracker.cancelled.load(Ordering::SeqCst)
            && self.tracker.record_error(None)
        {
            on_manga_download_complete(self.tracker, self.state);
        }
    }
}

/// Invalidates cached chapter page lists and CBZ paths for the given manga ID.
fn invalidate_manga_chapter_cache(id: &str, state: &AppState) {
    let prefix = format!("{}::", id);
    lock_mutex(&state.cache.chapter_pages).retain(|k, _| !k.starts_with(&prefix));
    lock_mutex(&state.cache.chapter_cbz_paths).retain(|k, _| !k.starts_with(&prefix));
}

/// Cancels any queued/active downloads for the given manga ID.
/// Returns `true` if an active download was found and cancelled, `false` otherwise.
pub fn cancel_download(id: &str, state: &AppState) -> bool {
    let tracker = lock_mutex(&state.active_downloads).remove(id);
    if let Some(tracker) = tracker {
        tracker.cancelled.store(true, Ordering::SeqCst);
        invalidate_manga_chapter_cache(id, state);
        info!("Cancelled active download for manga {}", id);
        true
    } else {
        false
    }
}

/// Contextual batch parameters required to queue and track manga chapter downloads.
#[derive(Debug)]
pub struct MangaDownloadBatch {
    pub manga: Manga,
    pub chapters: Vec<Chapter>,
    pub to_download: Vec<Chapter>,
    pub existing_count: usize,
    pub max_width: usize,
    pub output_dir: PathBuf,
}

/// Enqueues download tasks for the specified chapters of a manga.
/// Returns the number of chapters queued (0 if already in progress or empty).
pub fn queue_download_chapters(
    id: &str,
    state: &Arc<AppState>,
    batch: MangaDownloadBatch,
) -> usize {
    if batch.to_download.is_empty() {
        return 0;
    }

    let missing_len = batch.to_download.len();
    let total_chapters = batch.chapters.len();
    let existing_count = batch.existing_count;
    let url = format!("{}/manga/{}", *crate::config::BASE_URL, id);

    let tracker = {
        let mut active = lock_mutex(&state.active_downloads);
        if active.contains_key(id) {
            info!(
                "Download for {} is already queued or in progress. Skipping duplicate.",
                id
            );
            return 0;
        }
        let tracker = Arc::new(MangaDownloadTracker::new(
            id.to_string(),
            batch.manga,
            url,
            batch.chapters,
            batch.output_dir,
            batch.max_width,
            missing_len,
        ));
        active.insert(id.to_string(), Arc::clone(&tracker));
        tracker
    };

    let mut guard = DownloadGuard {
        id: id.to_string(),
        active_downloads: Arc::clone(&state.active_downloads),
        disarmed: false,
    };

    info!(
        "Queuing download of {} chapters for manga {}.",
        missing_len, id
    );

    let _ = upsert_manga(
        &lock_mutex(&state.db),
        id,
        &tracker.manga.title,
        &tracker.manga.status,
        total_chapters,
        Some(existing_count),
        CheckTrigger::UserRequest {
            new_chapters: false,
        },
    );

    let desc = truncate_str(&tracker.manga.description, 200);
    send_webhook(
        &state.client,
        NotificationType::Start {
            manga_title: &tracker.manga.title,
            manga_url: &tracker.url,
            description: &desc,
            total_chapters,
            existing_chapters: existing_count,
            missing_chapters: missing_len,
        },
    );

    for chapter in batch.to_download {
        let tracker = Arc::clone(&tracker);
        let state_clone = Arc::clone(state);

        state.download_pool.spawn(move || {
            if tracker.cancelled.load(Ordering::SeqCst) {
                return;
            }

            let mut guard = ChapterGuard {
                tracker: &tracker,
                state: &state_clone,
                completed: false,
            };

            let pb = indicatif::ProgressBar::hidden();
            let ctx = ChapterDownloadContext {
                client: &state_clone.client,
                manga: &tracker.manga,
                chapter: &chapter,
                output_dir: &tracker.output_dir,
                width: tracker.max_width,
                verify: false,
            };

            let res = download_chapter_with_retry(
                &ctx,
                &pb,
                &state_clone.rate_limiter,
                |cooldown, attempt, max_attempts| {
                    warn!(
                        "Rate limit encountered on chapter {} for {}. Pausing worker for {:?} before retry (attempt {}/{})",
                        chapter.number, tracker.manga.title, cooldown, attempt, max_attempts
                    );
                },
            );

            if tracker.cancelled.load(Ordering::SeqCst) {
                guard.completed = true;
                return;
            }

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
                on_manga_download_complete(&tracker, &state_clone);
            }
        });
    }

    // Disarm guard: all worker jobs are safely enqueued, tracker completion logic takes over
    guard.disarmed = true;

    missing_len
}

/// Initiates or queues a background download of all missing chapters for the given manga ID.
/// Used by cron auto-updates.
pub fn queue_background_download(
    id: &str,
    state: &Arc<AppState>,
    preloaded: Option<(Manga, Vec<Chapter>)>,
) {
    if lock_mutex(&state.active_downloads).contains_key(id) {
        info!(
            "Download for {} is already queued or in progress. Skipping duplicate.",
            id
        );
        return;
    }

    let id_str = id.to_string();
    let state_clone = Arc::clone(state);

    state.download_pool.spawn(move || {
        let Some((manga, mut chapters)) = fetch_manga_data(&id_str, &state_clone, preloaded) else {
            return;
        };

        let folder_name = get_folder_name(&manga.title);
        let manga_output_dir = state_clone.output_dir.join(&folder_name);

        lock_mutex(&state_clone.cache.manga_dirs)
            .entry(id_str.clone())
            .or_insert_with(|| (manga.title.clone(), manga_output_dir.clone()));

        let (max_width, existing_count) =
            scan_manga_chapters(&manga_output_dir, &manga.title, &mut chapters);

        let missing: Vec<Chapter> = chapters.iter().filter(|c| !c.downloaded).cloned().collect();

        if missing.is_empty() {
            let _ = upsert_manga(
                &lock_mutex(&state_clone.db),
                &id_str,
                &manga.title,
                &manga.status,
                chapters.len(),
                Some(existing_count),
                CheckTrigger::UserRequest {
                    new_chapters: false,
                },
            );
            info!("All chapters for {} already downloaded.", id_str);
            return;
        }

        queue_download_chapters(
            &id_str,
            &state_clone,
            MangaDownloadBatch {
                manga,
                chapters,
                to_download: missing,
                existing_count,
                max_width,
                output_dir: manga_output_dir,
            },
        );
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
    if tracker.cancelled.load(Ordering::SeqCst) {
        return;
    }

    let _guard = DownloadGuard {
        id: tracker.id.clone(),
        active_downloads: Arc::clone(&state.active_downloads),
        disarmed: false,
    };

    invalidate_manga_chapter_cache(&tracker.id, state);

    let total_successes = tracker.success_count.load(Ordering::Relaxed);
    let total_errors = tracker.error_count.load(Ordering::Relaxed);

    let mut chaps = tracker.chapters.clone();
    let (_, local_count) =
        scan_manga_chapters(&tracker.output_dir, &tracker.manga.title, &mut chaps);

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
