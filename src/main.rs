mod analytics;
mod app;
mod cache;
mod cli;
mod config;
mod delete;
mod filter;
mod format;
mod headless;
mod portable_path;
mod scanner;
mod snapshot;
mod theme;
mod tree;
mod ui;
mod watcher;
mod worker;

use std::fs;

use anyhow::{Context, Result, bail};
use clap::Parser;

use crate::{app::App, cli::Cli, config::Settings};

fn main() -> Result<()> {
    let cli = Cli::parse();
    let settings = Settings::load(&cli)?;
    let root = fs::canonicalize(&cli.path)
        .with_context(|| format!("Cannot open start path: {}", cli.path.display()))?;

    if !root.is_dir() {
        bail!("Start path is not a directory: {}", root.display());
    }

    if settings.export_json.is_some() {
        return headless::run(root, &settings);
    }

    let mut app = App::with_settings(root, settings)?;
    ratatui::run(|terminal| app.run(terminal))?;
    Ok(())
}
