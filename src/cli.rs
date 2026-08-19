use std::{
    io::{self, IsTerminal, Write},
    num::NonZeroUsize,
    path::PathBuf,
    process::{Command, Stdio},
};

use clap::{Parser, error::ErrorKind};

use crate::{
    theme::ThemeKind,
    tree::{SizeMode, SortDirection, SortKey},
};

const MANPAGE_HELP_TEMPLATE: &str = "\
MACDIRSTAT(1)

NAME
    macDirStat - {about}

SYNOPSIS
    {usage}

DESCRIPTION
    macDirStat is an interactive macOS terminal application for finding large files and
    directories. It scans directory metadata in background workers so that navigation,
    filtering, and cancellation remain responsive. The Tree view scans on demand; the
    analysis views build a complete file index for largest-file, type, duplicate, and
    change reports.

    Logical and physical sizes are collected together. Symbolic links are shown but never
    followed. By default, mounted filesystems below the start path remain visible but are
    not traversed. Deletions are confirmed and move entries to the macOS Trash.

    When standard output is an interactive terminal, this help page opens in less. Use the
    arrow keys, PageUp, or PageDown to scroll and q to quit. Redirected help is written as
    plain text without starting a pager.

ARGUMENTS
{positionals}
OPTIONS
{options}
{after-help}";

const MANPAGE_AFTER_HELP: &str = "\
INTERACTIVE KEYS
    Tab / Shift+Tab, 1-5
        Switch between Tree, Largest, Types, Duplicates, and Changes.
    Up / Down, k / j
        Move the selection.
    Right / Left, l / h
        Open a directory, or close it and move to its parent.
    Enter
        Expand or collapse a directory; reveal an analysis result in Tree.
    g / G, Home / End, PageUp / PageDown
        Jump to the first or last item, or move one page.
    Backspace
        Open the parent directory as the new scan root.
    /, n / N
        Search loaded entries and select the next or previous match.
    f / F
        Set or clear the combined filter.
    s / S
        Cycle the sort key or reverse the sort direction.
    z, i, t, m, w
        Toggle size mode, details, theme, mouse capture, or filesystem watching.
    Space, x
        Mark entries and move all marked entries to the Trash after confirmation.
    d
        Move the selected entry to the Trash after confirmation.
    r
        Discard cached data for the root and perform a fresh scan.
    e
        Export the completed analysis index as JSON.
    Esc
        Cancel running scan jobs or close the current dialog.
    ?, q, Ctrl+C
        Open interactive help, quit, or quit immediately.

FILTER SYNTAX
    Filter terms are combined with AND. Names and quoted phrases are accepted alongside
    predicates such as >1GiB, size>500MB, age>30d, ext:log, ext:none, and type:image.
    File categories are image, video, audio, archive, document, code, and other.

CONFIGURATION
    Settings are applied in this order: built-in defaults, TOML configuration, command-line
    options. Supported TOML keys are workers, size_mode, one_file_system, cache,
    cache_ttl_hours, mouse, theme, detail_panel, sort, sort_direction, watch,
    watch_debounce_ms, and snapshots.

FILES
    ~/Library/Application Support/macDirStat/config.toml
        Default configuration file.
    ~/Library/Caches/macDirStat/scan-cache-v1.json
        Persistent directory scan cache.
    ~/Library/Application Support/macDirStat/snapshots/
        Rolling snapshots used by the Changes view.

EXAMPLES
    Scan the current directory:
        macDirStat

    Scan Library using physical sizes and four workers:
        macDirStat --workers 4 --size-mode physical ~/Library

    Traverse filesystems mounted below the start path:
        macDirStat --cross-filesystems /

    Export a complete index to standard output with duplicate detection:
        macDirStat --export-json - --detect-duplicates ~/Library > scan.json

    Compare a new scan with an exported snapshot:
        macDirStat --export-json changes.json --compare-snapshot scan.json ~/Library

EXIT STATUS
    0   Successful completion.
    1   A runtime, filesystem, configuration, or export error occurred.
    2   Invalid command-line usage.
";

#[derive(Debug, Parser)]
#[command(
    name = "macDirStat",
    version = env!("MACDIRSTAT_VERSION"),
    about = "explore disk usage in a responsive terminal UI",
    help_template = MANPAGE_HELP_TEMPLATE,
    after_long_help = MANPAGE_AFTER_HELP,
    next_line_help = true,
    max_term_width = 100
)]
pub struct Cli {
    /// Directory to scan; defaults to the current directory.
    #[arg(default_value = ".", value_name = "PATH")]
    pub path: PathBuf,

    /// Read settings from PATH instead of the default TOML file.
    #[arg(long, value_name = "PATH")]
    pub config: Option<PathBuf>,

    /// Set the number of background filesystem workers; defaults to available CPUs, capped at 8.
    #[arg(long, value_name = "N")]
    pub workers: Option<NonZeroUsize>,

    /// Select logical file length or physical allocated space; defaults to logical.
    #[arg(long, value_enum)]
    pub size_mode: Option<SizeMode>,

    /// Stay on the filesystem containing PATH; this is the default unless configuration overrides it.
    #[arg(long, conflicts_with = "cross_filesystems")]
    pub one_file_system: bool,

    /// Traverse filesystems mounted below PATH.
    #[arg(long, conflicts_with = "one_file_system")]
    pub cross_filesystems: bool,

    /// Enable the persistent directory scan cache; enabled by default.
    #[arg(long, conflicts_with = "no_cache")]
    pub cache: bool,

    /// Disable reading and writing the persistent directory scan cache.
    #[arg(long, conflicts_with = "cache")]
    pub no_cache: bool,

    /// Set the maximum age of cache entries; defaults to 24 hours.
    #[arg(long, value_name = "HOURS")]
    pub cache_ttl_hours: Option<u64>,

    /// Enable mouse capture at startup; disabled by default.
    #[arg(long, conflicts_with = "no_mouse")]
    pub mouse: bool,

    /// Disable mouse capture at startup.
    #[arg(long, conflicts_with = "mouse")]
    pub no_mouse: bool,

    /// Select the initial color theme; defaults to default.
    #[arg(long, value_enum)]
    pub theme: Option<ThemeKind>,

    /// Show the adaptive detail panel; shown by default.
    #[arg(long, conflicts_with = "no_details")]
    pub details: bool,

    /// Hide the adaptive detail panel.
    #[arg(long, conflicts_with = "details")]
    pub no_details: bool,

    /// Select the initial sort key; defaults to size.
    #[arg(long, value_enum)]
    pub sort: Option<SortKey>,

    /// Select the initial sort direction; defaults to the natural direction of the sort key.
    #[arg(long, value_enum)]
    pub sort_direction: Option<SortDirection>,

    /// Watch the scanned tree for filesystem changes; disabled by default.
    #[arg(long, conflicts_with_all = ["no_watch", "export_json"])]
    pub watch: bool,

    /// Disable filesystem watching.
    #[arg(long, conflicts_with = "watch")]
    pub no_watch: bool,

    /// Enable rolling snapshots after a complete analysis; enabled by default.
    #[arg(long, conflicts_with = "no_snapshots")]
    pub snapshots: bool,

    /// Disable rolling snapshots and automatic Changes baselines.
    #[arg(long, conflicts_with = "snapshots")]
    pub no_snapshots: bool,

    /// Write a complete analysis as JSON and exit; use - for standard output.
    #[arg(long, value_name = "FILE", conflicts_with = "watch")]
    pub export_json: Option<PathBuf>,

    /// Compare the completed analysis against the snapshot in FILE.
    #[arg(long, value_name = "FILE")]
    pub compare_snapshot: Option<PathBuf>,

    /// Hash same-size candidates with BLAKE3 to identify exact duplicates.
    #[arg(long)]
    pub detect_duplicates: bool,
}

impl Cli {
    pub fn parse_with_help_pager() -> io::Result<Option<Self>> {
        match Self::try_parse() {
            Ok(cli) => Ok(Some(cli)),
            Err(error) if error.kind() == ErrorKind::DisplayHelp => {
                page_help(&error.to_string())?;
                Ok(None)
            }
            Err(error) => error.exit(),
        }
    }
}

fn page_help(help: &str) -> io::Result<()> {
    if !io::stdout().is_terminal() {
        return write_help_directly(help);
    }

    let mut child = match Command::new("less").arg("-R").stdin(Stdio::piped()).spawn() {
        Ok(child) => child,
        Err(_) => return write_help_directly(help),
    };

    let write_result = child
        .stdin
        .take()
        .expect("less was started with piped standard input")
        .write_all(help.as_bytes());
    let wait_result = child.wait();

    if let Err(error) = write_result
        && error.kind() != io::ErrorKind::BrokenPipe
    {
        return Err(error);
    }
    wait_result.map(|_| ())
}

fn write_help_directly(help: &str) -> io::Result<()> {
    let stdout = io::stdout();
    let mut output = stdout.lock();
    output.write_all(help.as_bytes())?;
    output.flush()
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
    fn long_help_is_a_complete_manpage() {
        let mut output = Vec::new();
        Cli::command().write_long_help(&mut output).unwrap();
        let help = String::from_utf8(output).unwrap();

        for section in [
            "MACDIRSTAT(1)",
            "NAME",
            "SYNOPSIS",
            "DESCRIPTION",
            "ARGUMENTS",
            "OPTIONS",
            "INTERACTIVE KEYS",
            "FILTER SYNTAX",
            "CONFIGURATION",
            "FILES",
            "EXAMPLES",
            "EXIT STATUS",
        ] {
            assert!(help.contains(section), "missing help section: {section}");
        }

        for option in [
            "--config",
            "--workers",
            "--size-mode",
            "--one-file-system",
            "--cross-filesystems",
            "--cache",
            "--no-cache",
            "--cache-ttl-hours",
            "--mouse",
            "--no-mouse",
            "--theme",
            "--details",
            "--no-details",
            "--sort",
            "--sort-direction",
            "--watch",
            "--no-watch",
            "--snapshots",
            "--no-snapshots",
            "--export-json",
            "--compare-snapshot",
            "--detect-duplicates",
            "--help",
            "--version",
        ] {
            assert!(help.contains(option), "missing documented option: {option}");
        }
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
        assert!(Cli::try_parse_from(["macDirStat", "--watch", "--export-json", "-"]).is_err());
    }

    #[test]
    fn parses_phase_three_options() {
        let cli = Cli::try_parse_from([
            "macDirStat",
            "--watch",
            "--snapshots",
            "--compare-snapshot",
            "before.json",
            "--detect-duplicates",
        ])
        .unwrap();
        assert!(cli.watch);
        assert!(cli.snapshots);
        assert_eq!(cli.compare_snapshot, Some(PathBuf::from("before.json")));
        assert!(cli.detect_duplicates);
    }
}
