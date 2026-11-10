use crate::{
    config::CRON_HOURS,
    db::{CheckTrigger, get_mangas_to_check, upsert_manga},
    scraper::manga_chapters,
    server::{downloader::queue_background_download, helpers::lock_mutex, state::AppState},
    utils::get_folder_name,
};
use log::{error, info, warn};
use std::{
    panic::{self, catch_unwind},
    sync::Arc,
    thread::sleep,
    time::Duration,
};

/// Periodic background task that checks for new chapters of stored mangas and queues downloads.
pub fn run_cron(state: &Arc<AppState>) {
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
                let url = format!("{}/manga/{}", *crate::config::BASE_URL, manga_chk.id);
                match manga_chapters(&state.client, &url) {
                    Ok((manga, mut chapters)) => {
                        let folder = get_folder_name(&manga.title);
                        let manga_dir = state.output_dir.join(&folder);
                        let (_, local_count) = crate::utils::scan_manga_chapters(
                            &manga_dir,
                            &manga.title,
                            &mut chapters,
                        );
                        let did_update = local_count < chapters.len();
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
