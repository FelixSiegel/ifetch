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

    let mut saved = 0;
    let chosen_len = chosen.len();
    for (i, chapter) in chosen.iter().enumerate() {
        if core::download_chapter(
            &client,
            &manga,
            chapter,
            &manga_output_dir,
            max_width,
            args.verify,
        )?
        .is_some()
        {
            saved += 1;
            if i + 1 < chosen_len {
                std::thread::sleep(Duration::from_millis(500));
            }
        }
    }

    let path = manga_output_dir.canonicalize().unwrap_or(manga_output_dir);
    println!("\nDone: {} new CBZ file(s) in {}", saved, path.display());

    Ok(())
}
