mod cli;
mod core;
mod models;
mod utils;

use crate::cli::Cli;
use clap::Parser;
use std::process;
use std::time::Duration;

fn main() {
    if let Err(e) = run() {
        eprintln!("Error: {:#}", e);
        process::exit(1);
    }
}

fn run() -> anyhow::Result<()> {
    let args = Cli::parse();

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
    if genres_str.chars().count() > 80 {
        genres_str = genres_str.chars().take(77).collect::<String>() + "...";
    }

    println!(
        "\n{}\n{}\n{}\n\n{} chapters ({} - {})",
        style(manga.title.clone()).cyan().bold(),
        style(genres_str).yellow(),
        style(manga.description.clone()).dim(),
        style(chapters.len()).green(),
        chapters.first().unwrap().number,
        chapters.last().unwrap().number
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

    // Clean title for folder name
    let folder_name = manga
        .title
        .replace(|c: char| r#"<>:"/\|?*"#.contains(c), "");
    let folder_name = folder_name.trim();
    let folder_name = if folder_name.is_empty() {
        "manga".to_string()
    } else {
        folder_name.to_string()
    };

    let manga_output_dir = args.output.join(folder_name);
    std::fs::create_dir_all(&manga_output_dir)?;

    let max_width = chapters
        .iter()
        .map(|c| c.number.to_string().split('.').next().unwrap().len())
        .max()
        .unwrap_or(3)
        .max(3);

    use indicatif::{MultiProgress, ProgressBar, ProgressStyle};
    use rayon::prelude::*;

    rayon::ThreadPoolBuilder::new()
        .num_threads(args.threads)
        .build_global()
        .unwrap_or(());

    let m = MultiProgress::new();
    let style = ProgressStyle::with_template("{msg} [{bar:40.cyan/blue}] {pos}/{len}")
        .unwrap()
        .progress_chars("=>-");

    let current_index = std::sync::atomic::AtomicUsize::new(0);
    let chosen_len = chosen.len();

    let saved: usize = (0..args.threads)
        .into_par_iter()
        .map(|_| {
            let mut local_saved = 0;
            loop {
                let i = current_index.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                if i >= chosen_len {
                    break;
                }
                let chapter = &chosen[i];

                let pb = m.add(ProgressBar::new(0));
                pb.set_style(style.clone());

                let res = core::download_chapter(
                    &client,
                    &manga,
                    chapter,
                    &manga_output_dir,
                    max_width,
                    args.verify,
                    &pb,
                );

                // short sleep after each downloaded chapter
                if let Ok(Some(_)) = res {
                    std::thread::sleep(Duration::from_millis(500));
                    local_saved += 1;
                }
            }
            local_saved
        })
        .sum();

    let path = manga_output_dir.canonicalize().unwrap_or(manga_output_dir);
    println!("\nDone: {} new CBZ file(s) in {}", saved, path.display());

    Ok(())
}
