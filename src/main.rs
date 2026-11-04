pub mod archive;
pub mod cli;
pub mod config;
pub mod db;
pub mod discord;
pub mod models;
pub mod rate_limit;
pub mod scraper;
pub mod server;
pub mod utils;

use crate::cli::Cli;
use clap::Parser;
use std::process;

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
        server::run_server(args.port, args.output, args.config, args.threads)
    } else {
        cli::run_download(args)
    }
}
