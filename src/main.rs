mod cli;
mod config;
mod core;
mod db;
mod discord;
mod models;
pub mod rate_limit;
mod server;
mod utils;

use crate::cli::Cli;
use crate::rate_limit::AdaptiveRateLimiter;
use crate::utils::truncate_str;
use clap::Parser;
use std::process;
use std::sync::Arc;

fn main() {
    if let Err(e) = run() {
        eprintln!("Error: {:#}", e);
        process::exit(1);
    }
}

fn run() -> anyhow::Result<()> {
    let _ = env_logger::try_init();

    let mut args = Cli::parse();
    args.threads = args.threads.max(1);

    if args.server {
        return server::run_server(args.port, args.output, args.config, args.threads);
    }

    let query = match args.manga {
        Some(m) => m,
        None => cli::prompt_manga()?,
    };

    let client = core::build_client()?;

    let direct_url = utils::series_url(&query)?;
    let (manga, chapters) = if let Some(url) = direct_url {
        core::manga_chapters(&client, &url)?
    } else {
        let results = core::search_manga(&client, &query)?;
        let selected = cli::choose_manga(&results)?;
        core::manga_chapters(&client, &selected.url)?
    };

    use console::style;

    let mut genres_str = manga.genres.join(", ");
    genres_str = truncate_str(&genres_str, 80);

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

    if args.list {
        for ch in &chapters {
            println!("{:>8}  {}", ch.number, ch.label);
        }
        return Ok(());
    }

    let spec = match args.chapters {
        Some(c) => c,
        None => cli::prompt_chapters()?,
    };

    let chosen = core::select_chapters(&chapters, &spec)?;

    let folder_name = crate::utils::get_folder_name(&manga.title);
    let manga_output_dir = args.output.join(folder_name);
    let (max_width, _, _) =
        crate::utils::scan_manga_chapters(&manga_output_dir, &manga.title, &chapters);

    use indicatif::{MultiProgress, ProgressBar, ProgressStyle};

    let m = MultiProgress::new();
    let style = ProgressStyle::with_template("{msg} [{bar:40.cyan/blue}] {pos}/{len}")
        .unwrap()
        .progress_chars("=>-");

    let rate_limiter = Arc::new(AdaptiveRateLimiter::new());
    let current_index = std::sync::atomic::AtomicUsize::new(0);
    let chosen_len = chosen.len();

    let saved: usize = std::thread::scope(|s| {
        let num_workers = args.threads.min(chosen_len);
        let mut handles = Vec::with_capacity(args.threads);
        for _ in 0..num_workers {
            handles.push(s.spawn(|| {
                let mut local_saved = 0;
                loop {
                    let i = current_index.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                    if i >= chosen_len {
                        break;
                    }
                    let chapter = &chosen[i];

                    let pb = m.add(ProgressBar::new(0));
                    pb.set_style(style.clone());

                    let ctx = core::ChapterDownloadContext {
                        client: &client,
                        manga: &manga,
                        chapter,
                        output_dir: &manga_output_dir,
                        width: max_width,
                        verify: args.verify,
                    };

                    let res = core::download_chapter_with_retry(
                        &ctx,
                        &pb,
                        &rate_limiter,
                        |cooldown, attempt, max_attempts| {
                            pb.set_message(format!(
                                "Rate limited on Ch {}. Pausing {:?} (attempt {}/{})",
                                chapter.number, cooldown, attempt, max_attempts
                            ));
                        },
                    );

                    match res {
                        Ok(Some(_)) => {
                            local_saved += 1;
                        }
                        Ok(None) => {}
                        Err(e) => {
                            let _ = m.println(format!(
                                "Failed to download chapter {}: {:#}",
                                chapter.number, e
                            ));
                        }
                    }
                }
                local_saved
            }));
        }
        handles.into_iter().map(|h| h.join().unwrap_or(0)).sum()
    });

    let path = manga_output_dir.canonicalize().unwrap_or(manga_output_dir);
    println!("\nDone: {} new CBZ file(s) in {}", saved, path.display());

    Ok(())
}
