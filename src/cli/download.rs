use crate::{
    archive::{self, ChapterDownloadContext},
    cli::{Cli, choose_manga, prompt_chapters, prompt_manga, selector::select_chapters},
    models::{Chapter, Manga},
    rate_limit::AdaptiveRateLimiter,
    scraper,
    utils::{get_folder_name, scan_manga_chapters, series_url, truncate_str},
};
use console::style;
use indicatif::{MultiProgress, ProgressBar, ProgressStyle};
use reqwest::blocking::Client;
use std::{
    path::Path,
    sync::atomic::{AtomicUsize, Ordering},
};

/// Resolves a manga and its chapters from a direct URL or an interactive search query.
pub fn resolve_series(client: &Client, query: &str) -> anyhow::Result<(Manga, Vec<Chapter>)> {
    if let Some(url) = series_url(query)? {
        scraper::manga_chapters(client, &url)
    } else {
        let results = scraper::search_manga(client, query)?;
        let selected = choose_manga(&results)?;
        scraper::manga_chapters(client, &selected.url)
    }
}

/// Prints a stylized terminal summary of the manga metadata and chapter range.
pub fn print_manga_summary(manga: &Manga, chapters: &[Chapter]) {
    let genres_str = truncate_str(&manga.genres.join(", "), 80);
    let chapter_summary = match (chapters.first(), chapters.last()) {
        (Some(first), Some(last)) => format!(" ({} - {})", first.number, last.number),
        _ => String::new(),
    };

    println!(
        "\n{}\n{}\n{}\n\n{} chapters{}",
        style(&manga.title).cyan().bold(),
        style(genres_str).yellow(),
        style(&manga.description).dim(),
        style(chapters.len()).green(),
        chapter_summary
    );
}

struct CliDownloadSession<'a> {
    client: &'a Client,
    manga: &'a Manga,
    output_dir: &'a Path,
    max_width: usize,
    verify: bool,
    multi: &'a MultiProgress,
    style: &'a ProgressStyle,
    rate_limiter: &'a AdaptiveRateLimiter,
}

impl<'a> CliDownloadSession<'a> {
    fn download_chapter(&self, chapter: &Chapter) -> bool {
        let pb = self.multi.add(ProgressBar::new(0));
        pb.set_style(self.style.clone());

        let ctx = ChapterDownloadContext {
            client: self.client,
            manga: self.manga,
            chapter,
            output_dir: self.output_dir,
            width: self.max_width,
            verify: self.verify,
        };

        let res = archive::download_chapter_with_retry(
            &ctx,
            &pb,
            self.rate_limiter,
            |cooldown, attempt, max_attempts| {
                pb.set_message(format!(
                    "Rate limited on Ch {}. Pausing {:?} (attempt {}/{})",
                    chapter.number, cooldown, attempt, max_attempts
                ));
            },
        );

        match res {
            Ok(Some(_)) => true,
            Ok(None) => false,
            Err(e) => {
                let _ = self.multi.println(format!(
                    "Failed to download chapter {}: {:#}",
                    chapter.number, e
                ));
                false
            }
        }
    }

    /// Worker thread loop that downloads chapters until all have been processed.
    fn run_worker(&self, chosen: &[Chapter], index: &AtomicUsize) -> usize {
        std::iter::from_fn(|| chosen.get(index.fetch_add(1, Ordering::Relaxed)))
            .filter(|chapter| self.download_chapter(chapter))
            .count()
    }
}

/// Downloads selected chapters concurrently using a scoped thread pool and progress bars.
pub fn download_chapters(
    client: &Client,
    manga: &Manga,
    chosen: &[Chapter],
    output_dir: &Path,
    max_width: usize,
    threads: usize,
    verify: bool,
) -> usize {
    let multi = MultiProgress::new();
    let style = ProgressStyle::with_template("{msg} [{bar:40.cyan/blue}] {pos}/{len}")
        .unwrap()
        .progress_chars("=>-");

    let rate_limiter = AdaptiveRateLimiter::new();
    let session = CliDownloadSession {
        client,
        manga,
        output_dir,
        max_width,
        verify,
        multi: &multi,
        style: &style,
        rate_limiter: &rate_limiter,
    };

    let current_index = AtomicUsize::new(0);
    let num_workers = threads.min(chosen.len());

    std::thread::scope(|s| {
        let handles: Vec<_> = (0..num_workers)
            .map(|_| s.spawn(|| session.run_worker(chosen, &current_index)))
            .collect();

        handles.into_iter().map(|h| h.join().unwrap_or(0)).sum()
    })
}

/// Executes the interactive CLI download workflow.
pub fn run_download(args: Cli) -> anyhow::Result<()> {
    let query = match args.manga {
        Some(m) => m,
        None => prompt_manga()?,
    };

    let client = scraper::build_client()?;
    let (manga, mut chapters) = resolve_series(&client, &query)?;

    print_manga_summary(&manga, &chapters);

    if args.list {
        for ch in &chapters {
            println!("{:>8}  {}", ch.number, ch.label);
        }
        return Ok(());
    }

    let spec = match args.chapters {
        Some(c) => c,
        None => prompt_chapters()?,
    };

    let chosen = select_chapters(&chapters, &spec)?;

    let folder_name = get_folder_name(&manga.title);
    let manga_output_dir = args.output.join(folder_name);
    let (max_width, _) = scan_manga_chapters(&manga_output_dir, &manga.title, &mut chapters);

    let saved = download_chapters(
        &client,
        &manga,
        &chosen,
        &manga_output_dir,
        max_width,
        args.threads,
        args.verify,
    );

    let path = manga_output_dir.canonicalize().unwrap_or(manga_output_dir);
    println!("\nDone: {} new CBZ file(s) in {}", saved, path.display());

    Ok(())
}
