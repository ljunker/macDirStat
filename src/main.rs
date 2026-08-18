mod app;
mod cli;
mod delete;
mod format;
mod scanner;
mod tree;
mod ui;
mod worker;

use std::fs;

use anyhow::{Context, Result, bail};
use clap::Parser;

use crate::{app::App, cli::Cli};

fn main() -> Result<()> {
    let cli = Cli::parse();
    let root = fs::canonicalize(&cli.path)
        .with_context(|| format!("Cannot open start path: {}", cli.path.display()))?;

    if !root.is_dir() {
        bail!("Start path is not a directory: {}", root.display());
    }

    let workers = cli.worker_count();
    let mut app = App::new(root, workers)?;
    ratatui::run(|terminal| app.run(terminal))?;
    Ok(())
}
