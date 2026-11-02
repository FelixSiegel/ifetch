pub mod cbz;
pub mod comic_info;
pub mod image;

pub use cbz::*;
pub use comic_info::*;
pub use image::*;

use crate::{
    models::{Chapter, Manga},
    rate_limit::{self, AdaptiveRateLimiter, is_rate_limit_error},
    scraper::chapter_images,
    utils::chapter_filename,
};
use anyhow::Result;
use indicatif::ProgressBar;
use reqwest::blocking::Client;
use std::{
    path::{Path, PathBuf},
    time::Duration,
};

/// Parameters and context required for downloading and packaging a single chapter.
pub struct ChapterDownloadContext<'a> {
    pub client: &'a Client,
    pub manga: &'a Manga,
    pub chapter: &'a Chapter,
    pub output_dir: &'a Path,
    pub width: usize,
    pub verify: bool,
}

/// Downloads all pages for a chapter, builds a `ComicInfo.xml` metadata file,
/// and packages the content into a CBZ archive.
pub fn download_chapter(ctx: &ChapterDownloadContext, pb: &ProgressBar) -> Result<Option<PathBuf>> {
    std::fs::create_dir_all(ctx.output_dir)?;
    let filename = chapter_filename(&ctx.manga.title, &ctx.chapter.number.to_string(), ctx.width);
    let dest = ctx.output_dir.join(&filename);

    if !ctx.verify && dest.exists() {
        pb.finish_with_message(format!("Skipped Chapter {}", ctx.chapter.number));
        return Ok(None);
    }

    pb.set_message(format!("Chapter {}...", ctx.chapter.number));
    let urls = chapter_images(ctx.client, &ctx.chapter.url)?;

    if dest.exists() {
        if is_chapter_up_to_date(&dest, urls.len()) {
            pb.finish_with_message(format!("Skipped Chapter {}", ctx.chapter.number));
            return Ok(None);
        }
        pb.set_message(format!(
            "Updating Ch {} ({} pages)",
            ctx.chapter.number,
            urls.len()
        ));
    } else {
        pb.set_message(format!(
            "Chapter {} ({} pages)",
            ctx.chapter.number,
            urls.len()
        ));
    }

    pb.set_length(urls.len() as u64);

    match write_cbz_archive(&dest, ctx.manga, ctx.chapter, &urls, ctx.client, pb) {
        Ok(_) => {
            pb.finish_with_message(format!("Saved Chapter {}", ctx.chapter.number));
            Ok(Some(dest))
        }
        Err(e) => {
            pb.abandon_with_message(format!("Error Chapter {}", ctx.chapter.number));
            Err(e)
        }
    }
}

/// Downloads a chapter with rate limiter pacing, automatic rate limit backoff retry, and success tracking.
pub fn download_chapter_with_retry<F>(
    ctx: &ChapterDownloadContext,
    pb: &ProgressBar,
    rate_limiter: &AdaptiveRateLimiter,
    on_retry: F,
) -> Result<Option<PathBuf>>
where
    F: Fn(Duration, usize, usize),
{
    const MAX_ATTEMPTS: usize = 3;

    for attempt in 1..=MAX_ATTEMPTS {
        rate_limiter.wait_for_chapter_permit();

        match download_chapter(ctx, pb) {
            Ok(val) => {
                rate_limiter.on_chapter_success();
                return Ok(val);
            }
            Err(e) if is_rate_limit_error(&e) => {
                let retry_after = rate_limit::extract_retry_after(&e);
                let cooldown =
                    rate_limiter.on_rate_limit_hit_with_retry(&e.to_string(), retry_after);
                if attempt < MAX_ATTEMPTS {
                    on_retry(cooldown, attempt, MAX_ATTEMPTS);
                } else {
                    return Err(e);
                }
            }
            Err(e) => return Err(e),
        }
    }

    unreachable!()
}
