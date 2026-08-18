use std::{num::NonZeroUsize, path::PathBuf, thread};

use clap::Parser;

#[derive(Debug, Parser)]
#[command(
    name = "macDirStat",
    version = env!("MACDIRSTAT_VERSION"),
    about = "Explore directory sizes in a responsive terminal UI"
)]
pub struct Cli {
    /// Directory to scan
    #[arg(default_value = ".")]
    pub path: PathBuf,

    /// Number of background filesystem workers
    #[arg(long, value_name = "N")]
    workers: Option<NonZeroUsize>,
}

impl Cli {
    pub fn worker_count(&self) -> usize {
        self.workers
            .map_or_else(default_worker_count, NonZeroUsize::get)
    }
}

fn default_worker_count() -> usize {
    thread::available_parallelism()
        .map(NonZeroUsize::get)
        .unwrap_or(1)
        .min(8)
}

#[cfg(test)]
mod tests {
    use super::*;
    use clap::CommandFactory;

    #[test]
    fn reports_version_from_version_file() {
        assert_eq!(
            Cli::command().get_version(),
            Some(env!("MACDIRSTAT_VERSION"))
        );
    }

    #[test]
    fn defaults_to_current_directory() {
        let cli = Cli::try_parse_from(["macDirStat"]).expect("CLI should parse");
        assert_eq!(cli.path, PathBuf::from("."));
        assert!((1..=8).contains(&cli.worker_count()));
    }

    #[test]
    fn parses_path_and_workers() {
        let cli = Cli::try_parse_from(["macDirStat", "--workers", "3", "/tmp"])
            .expect("CLI should parse");
        assert_eq!(cli.path, PathBuf::from("/tmp"));
        assert_eq!(cli.worker_count(), 3);
    }

    #[test]
    fn rejects_zero_workers() {
        assert!(Cli::try_parse_from(["macDirStat", "--workers", "0"]).is_err());
    }
}
