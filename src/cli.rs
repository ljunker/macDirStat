use std::{num::NonZeroUsize, path::PathBuf};

use clap::Parser;

use crate::{
    theme::ThemeKind,
    tree::{SizeMode, SortDirection, SortKey},
};

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

    /// Read settings from this TOML file
    #[arg(long, value_name = "PATH")]
    pub config: Option<PathBuf>,

    /// Number of background filesystem workers
    #[arg(long, value_name = "N")]
    pub workers: Option<NonZeroUsize>,

    /// Initial size display mode
    #[arg(long, value_enum)]
    pub size_mode: Option<SizeMode>,

    /// Stay on the filesystem containing the start path
    #[arg(long, conflicts_with = "cross_filesystems")]
    pub one_file_system: bool,

    /// Traverse mounted filesystems below the start path
    #[arg(long, conflicts_with = "one_file_system")]
    pub cross_filesystems: bool,

    /// Enable the persistent scan cache
    #[arg(long, conflicts_with = "no_cache")]
    pub cache: bool,

    /// Disable the persistent scan cache
    #[arg(long, conflicts_with = "cache")]
    pub no_cache: bool,

    /// Cache lifetime in hours
    #[arg(long, value_name = "HOURS")]
    pub cache_ttl_hours: Option<u64>,

    /// Enable mouse capture at startup
    #[arg(long, conflicts_with = "no_mouse")]
    pub mouse: bool,

    /// Disable mouse capture at startup
    #[arg(long, conflicts_with = "mouse")]
    pub no_mouse: bool,

    /// Initial color theme
    #[arg(long, value_enum)]
    pub theme: Option<ThemeKind>,

    /// Show the adaptive detail panel
    #[arg(long, conflicts_with = "no_details")]
    pub details: bool,

    /// Hide the detail panel
    #[arg(long, conflicts_with = "details")]
    pub no_details: bool,

    /// Initial sort key
    #[arg(long, value_enum)]
    pub sort: Option<SortKey>,

    /// Initial sort direction
    #[arg(long, value_enum)]
    pub sort_direction: Option<SortDirection>,
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
        assert!(cli.workers.is_none());
    }

    #[test]
    fn parses_phase_two_options() {
        let cli = Cli::try_parse_from([
            "macDirStat",
            "--workers",
            "3",
            "--size-mode",
            "physical",
            "--cross-filesystems",
            "--no-cache",
            "--mouse",
            "--theme",
            "monochrome",
            "/tmp",
        ])
        .expect("CLI should parse");
        assert_eq!(cli.path, PathBuf::from("/tmp"));
        assert_eq!(cli.workers.unwrap().get(), 3);
        assert_eq!(cli.size_mode, Some(SizeMode::Physical));
        assert!(cli.cross_filesystems);
        assert!(cli.no_cache);
        assert!(cli.mouse);
        assert_eq!(cli.theme, Some(ThemeKind::Monochrome));
    }

    #[test]
    fn rejects_zero_workers_and_conflicting_flags() {
        assert!(Cli::try_parse_from(["macDirStat", "--workers", "0"]).is_err());
        assert!(Cli::try_parse_from(["macDirStat", "--mouse", "--no-mouse"]).is_err());
    }
}
