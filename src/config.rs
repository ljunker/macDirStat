use std::{fs, num::NonZeroUsize, path::PathBuf, thread, time::Duration};

use anyhow::{Context, Result, bail};
use directories::ProjectDirs;
use serde::Deserialize;

use crate::{
    cli::Cli,
    theme::ThemeKind,
    tree::{SizeMode, SortDirection, SortKey, SortSpec},
};

const DEFAULT_CACHE_TTL_HOURS: u64 = 24;

#[derive(Clone, Debug)]
pub struct AppPaths {
    pub config_file: PathBuf,
    pub cache_file: PathBuf,
}

impl AppPaths {
    pub fn discover(config_override: Option<PathBuf>) -> Result<Self> {
        let project = ProjectDirs::from("", "", "macDirStat")
            .context("Could not determine macDirStat configuration directories")?;
        Ok(Self {
            config_file: config_override
                .unwrap_or_else(|| project.config_dir().join("config.toml")),
            cache_file: project.cache_dir().join("scan-cache-v1.json"),
        })
    }
}

#[derive(Clone, Debug)]
pub struct Settings {
    pub workers: usize,
    pub size_mode: SizeMode,
    pub one_file_system: bool,
    pub cache_enabled: bool,
    pub cache_ttl: Duration,
    pub mouse: bool,
    pub theme: ThemeKind,
    pub detail_panel: bool,
    pub sort: SortSpec,
    pub paths: AppPaths,
}

#[derive(Debug, Default, Deserialize)]
#[serde(default, deny_unknown_fields)]
struct FileConfig {
    workers: Option<usize>,
    size_mode: Option<SizeMode>,
    one_file_system: Option<bool>,
    cache: Option<bool>,
    cache_ttl_hours: Option<u64>,
    mouse: Option<bool>,
    theme: Option<ThemeKind>,
    detail_panel: Option<bool>,
    sort: Option<SortKey>,
    sort_direction: Option<SortDirection>,
}

impl Settings {
    pub fn load(cli: &Cli) -> Result<Self> {
        let paths = AppPaths::discover(cli.config.clone())?;
        let config = if paths.config_file.exists() {
            let contents = fs::read_to_string(&paths.config_file).with_context(|| {
                format!("Could not read config {}", paths.config_file.display())
            })?;
            toml::from_str::<FileConfig>(&contents)
                .with_context(|| format!("Invalid config {}", paths.config_file.display()))?
        } else if cli.config.is_some() {
            bail!(
                "Explicit config file does not exist: {}",
                paths.config_file.display()
            );
        } else {
            FileConfig::default()
        };

        let workers = cli
            .workers
            .map(NonZeroUsize::get)
            .or(config.workers)
            .unwrap_or_else(default_worker_count);
        if workers == 0 {
            bail!("workers must be greater than zero");
        }
        let cache_ttl_hours = cli
            .cache_ttl_hours
            .or(config.cache_ttl_hours)
            .unwrap_or(DEFAULT_CACHE_TTL_HOURS);
        if cache_ttl_hours == 0 {
            bail!("cache_ttl_hours must be greater than zero");
        }
        let sort_key = cli.sort.or(config.sort).unwrap_or_default();
        let sort_direction = cli
            .sort_direction
            .or(config.sort_direction)
            .unwrap_or_else(|| sort_key.default_direction());

        Ok(Self {
            workers,
            size_mode: cli.size_mode.or(config.size_mode).unwrap_or_default(),
            one_file_system: flag_pair(
                cli.one_file_system,
                cli.cross_filesystems,
                config.one_file_system.unwrap_or(true),
            ),
            cache_enabled: flag_pair(cli.cache, cli.no_cache, config.cache.unwrap_or(true)),
            cache_ttl: Duration::from_secs(cache_ttl_hours.saturating_mul(60 * 60)),
            mouse: flag_pair(cli.mouse, cli.no_mouse, config.mouse.unwrap_or(false)),
            theme: cli.theme.or(config.theme).unwrap_or_default(),
            detail_panel: flag_pair(
                cli.details,
                cli.no_details,
                config.detail_panel.unwrap_or(true),
            ),
            sort: SortSpec {
                key: sort_key,
                direction: sort_direction,
            },
            paths,
        })
    }

    #[cfg(test)]
    pub fn for_tests(workers: usize, root: &std::path::Path) -> Self {
        Self {
            workers,
            size_mode: SizeMode::Logical,
            one_file_system: true,
            cache_enabled: false,
            cache_ttl: Duration::from_secs(DEFAULT_CACHE_TTL_HOURS * 60 * 60),
            mouse: false,
            theme: ThemeKind::Default,
            detail_panel: true,
            sort: SortSpec::default(),
            paths: AppPaths {
                config_file: root.join("config.toml"),
                cache_file: root.join("cache.json"),
            },
        }
    }
}

fn flag_pair(positive: bool, negative: bool, fallback: bool) -> bool {
    if positive {
        true
    } else if negative {
        false
    } else {
        fallback
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
    use clap::Parser;
    use tempfile::tempdir;

    use super::*;

    #[test]
    fn empty_config_uses_phase_two_defaults() {
        let directory = tempdir().unwrap();
        let config_path = directory.path().join("settings.toml");
        fs::write(&config_path, "").unwrap();
        let cli =
            Cli::try_parse_from(["macDirStat", "--config", config_path.to_str().unwrap()]).unwrap();

        let settings = Settings::load(&cli).unwrap();
        assert_eq!(settings.size_mode, SizeMode::Logical);
        assert!(settings.one_file_system);
        assert!(settings.cache_enabled);
        assert_eq!(settings.cache_ttl, Duration::from_secs(24 * 60 * 60));
        assert!(!settings.mouse);
        assert_eq!(settings.theme, ThemeKind::Default);
        assert!(settings.detail_panel);
        assert_eq!(settings.sort.key, SortKey::Size);
        assert_eq!(settings.sort.direction, SortDirection::Descending);
    }

    #[test]
    fn cli_overrides_toml_and_defaults() {
        let directory = tempdir().unwrap();
        let config_path = directory.path().join("settings.toml");
        fs::write(
            &config_path,
            r#"
workers = 2
size_mode = "physical"
one_file_system = false
mouse = true
sort = "name"
"#,
        )
        .unwrap();
        let cli = Cli::try_parse_from([
            "macDirStat",
            "--config",
            config_path.to_str().unwrap(),
            "--workers",
            "3",
            "--size-mode",
            "logical",
            "--one-file-system",
            "--no-mouse",
        ])
        .unwrap();

        let settings = Settings::load(&cli).unwrap();
        assert_eq!(settings.workers, 3);
        assert_eq!(settings.size_mode, SizeMode::Logical);
        assert!(settings.one_file_system);
        assert!(!settings.mouse);
        assert_eq!(settings.sort.key, SortKey::Name);
        assert_eq!(settings.sort.direction, SortDirection::Ascending);
    }

    #[test]
    fn rejects_unknown_config_keys() {
        let directory = tempdir().unwrap();
        let config_path = directory.path().join("settings.toml");
        fs::write(&config_path, "unknown = true").unwrap();
        let cli =
            Cli::try_parse_from(["macDirStat", "--config", config_path.to_str().unwrap()]).unwrap();
        assert!(Settings::load(&cli).is_err());
    }

    #[test]
    fn rejects_invalid_config_values() {
        let directory = tempdir().unwrap();
        let config_path = directory.path().join("settings.toml");
        let cli =
            Cli::try_parse_from(["macDirStat", "--config", config_path.to_str().unwrap()]).unwrap();

        fs::write(&config_path, "workers = 0").unwrap();
        assert!(Settings::load(&cli).is_err());

        fs::write(&config_path, "theme = \"ultraviolet\"").unwrap();
        assert!(Settings::load(&cli).is_err());
    }
}
