use crate::models::Manga;
use anyhow::{Result, bail};
use clap::Parser;
use dialoguer::{Input, Select};
use std::path::PathBuf;

#[derive(Parser, Debug)]
#[command(name = "ifetch")]
#[command(about = "Download MangaKatana chapters as CBZ files.", long_about = None)]
pub struct Cli {
    /// Search text or MangaKatana URL
    pub manga: Option<String>,

    /// Chapter selection: 1-10, 1,3,5.5, or all
    #[arg(short, long)]
    pub chapters: Option<String>,

    /// Output directory
    #[arg(short, long, default_value = "downloads")]
    pub output: PathBuf,

    /// List chapters without downloading
    #[arg(long)]
    pub list: bool,
}

pub fn choose_manga(results: &[Manga]) -> Result<Manga> {
    if results.is_empty() {
        bail!("No manga found");
    }
    if results.len() == 1 {
        return Ok(results[0].clone());
    }

    let items: Vec<String> = results.iter().take(10).map(|m| m.title.clone()).collect();
    let selection = Select::new()
        .with_prompt("Select manga")
        .items(&items)
        .default(0)
        .interact()?;

    Ok(results[selection].clone())
}

pub fn prompt_manga() -> Result<String> {
    let input: String = Input::new()
        .with_prompt("Manga title or MangaKatana URL")
        .interact_text()?;
    Ok(input)
}

pub fn prompt_chapters() -> Result<String> {
    let input: String = Input::new()
        .with_prompt("Chapters [all]")
        .allow_empty(true)
        .interact_text()?;
    if input.trim().is_empty() {
        Ok("all".to_string())
    } else {
        Ok(input)
    }
}
