use std::{
    ffi::OsString,
    fs, io,
    path::{Path, PathBuf},
};

use walkdir::WalkDir;

use crate::tree::NodeKind;

#[derive(Debug)]
pub struct DiscoveredEntry {
    pub path: PathBuf,
    pub name: OsString,
    pub kind: NodeKind,
    pub size: Option<u64>,
    pub error: Option<String>,
}

#[derive(Clone, Debug, Default)]
pub struct ScanWarnings {
    pub error_count: usize,
    pub permission_denied: usize,
    pub first_message: Option<String>,
}

impl ScanWarnings {
    fn record_io(&mut self, path: &Path, error: &io::Error) {
        let message = if error.kind() == io::ErrorKind::PermissionDenied {
            self.permission_denied += 1;
            format!("Permission denied: {}", path.display())
        } else {
            format!("{}: {error}", path.display())
        };
        self.record_message(message);
    }

    fn record_message(&mut self, message: String) {
        self.error_count += 1;
        if self.first_message.is_none() {
            self.first_message = Some(message);
        }
    }
}

#[derive(Debug)]
pub struct LoadOutcome {
    pub entries: Vec<DiscoveredEntry>,
    pub warnings: ScanWarnings,
    pub cancelled: bool,
}

#[derive(Debug)]
pub struct ScanOutcome {
    pub size: u64,
    pub warnings: ScanWarnings,
    pub fatal_error: Option<String>,
    pub cancelled: bool,
}

pub fn load_children<F>(path: &Path, is_cancelled: F) -> io::Result<LoadOutcome>
where
    F: Fn() -> bool,
{
    let directory = fs::read_dir(path)?;
    let mut entries = Vec::new();
    let mut warnings = ScanWarnings::default();

    for item in directory {
        if is_cancelled() {
            return Ok(LoadOutcome {
                entries,
                warnings,
                cancelled: true,
            });
        }

        let entry = match item {
            Ok(entry) => entry,
            Err(error) => {
                warnings.record_io(path, &error);
                continue;
            }
        };
        let entry_path = entry.path();
        let name = entry.file_name();

        match fs::symlink_metadata(&entry_path) {
            Ok(metadata) => {
                let file_type = metadata.file_type();
                let kind = if file_type.is_dir() {
                    NodeKind::Directory
                } else if file_type.is_file() {
                    NodeKind::File
                } else if file_type.is_symlink() {
                    NodeKind::Symlink
                } else {
                    NodeKind::Other
                };
                let size = match kind {
                    NodeKind::Directory => None,
                    NodeKind::File | NodeKind::Symlink | NodeKind::Other => Some(metadata.len()),
                };
                entries.push(DiscoveredEntry {
                    path: entry_path,
                    name,
                    kind,
                    size,
                    error: None,
                });
            }
            Err(error) => {
                warnings.record_io(&entry_path, &error);
                entries.push(DiscoveredEntry {
                    path: entry_path,
                    name,
                    kind: NodeKind::Other,
                    size: None,
                    error: Some(error.to_string()),
                });
            }
        }
    }

    Ok(LoadOutcome {
        entries,
        warnings,
        cancelled: false,
    })
}

pub fn calculate_size<F>(path: &Path, is_cancelled: F) -> ScanOutcome
where
    F: Fn() -> bool,
{
    let mut size = 0_u64;
    let mut warnings = ScanWarnings::default();
    let mut fatal_error = None;

    for item in WalkDir::new(path).follow_links(false) {
        if is_cancelled() {
            return ScanOutcome {
                size,
                warnings,
                fatal_error,
                cancelled: true,
            };
        }

        let entry = match item {
            Ok(entry) => entry,
            Err(error) => {
                let error_path = error.path().unwrap_or(path);
                let message = error
                    .io_error()
                    .map_or_else(|| error.to_string(), ToString::to_string);
                if error.depth() == 0 {
                    fatal_error = Some(format!("{}: {message}", error_path.display()));
                }
                if let Some(io_error) = error.io_error() {
                    warnings.record_io(error_path, io_error);
                } else {
                    warnings.record_message(format!("{}: {message}", error_path.display()));
                }
                if fatal_error.is_some() {
                    break;
                }
                continue;
            }
        };

        if entry.depth() == 0 && entry.file_type().is_dir() {
            continue;
        }
        if entry.file_type().is_dir() {
            continue;
        }
        if !(entry.file_type().is_file() || entry.file_type().is_symlink()) {
            continue;
        }

        match fs::symlink_metadata(entry.path()) {
            Ok(metadata) => size = size.saturating_add(metadata.len()),
            Err(error) => warnings.record_io(entry.path(), &error),
        }
    }

    ScanOutcome {
        size,
        warnings,
        fatal_error,
        cancelled: false,
    }
}

#[cfg(test)]
mod tests {
    use std::fs;

    use tempfile::tempdir;

    use super::*;

    #[test]
    fn sums_regular_files_recursively() {
        let root = tempdir().unwrap();
        let nested = root.path().join("nested");
        fs::create_dir(&nested).unwrap();
        fs::write(root.path().join("one"), vec![0_u8; 10]).unwrap();
        fs::write(nested.join("two"), vec![0_u8; 25]).unwrap();

        let outcome = calculate_size(root.path(), || false);
        assert_eq!(outcome.size, 35);
        assert_eq!(outcome.warnings.error_count, 0);
        assert!(!outcome.cancelled);
    }

    #[cfg(unix)]
    #[test]
    fn does_not_follow_symlinks() {
        use std::os::unix::fs::symlink;

        let workspace = tempdir().unwrap();
        let scan_root = workspace.path().join("scan");
        let outside = workspace.path().join("outside");
        fs::create_dir(&scan_root).unwrap();
        fs::create_dir(&outside).unwrap();
        fs::write(outside.join("large"), vec![0_u8; 16 * 1024]).unwrap();
        let link = scan_root.join("outside-link");
        symlink(&outside, &link).unwrap();

        let outcome = calculate_size(&scan_root, || false);
        assert_eq!(outcome.size, fs::symlink_metadata(link).unwrap().len());
    }

    #[test]
    fn missing_root_becomes_a_fatal_scan_error() {
        let root = tempdir().unwrap();
        let missing = root.path().join("gone");
        let outcome = calculate_size(&missing, || false);
        assert!(outcome.fatal_error.is_some());
        assert!(outcome.warnings.error_count > 0);
    }

    #[test]
    fn cancellation_stops_directory_loading() {
        let root = tempdir().unwrap();
        fs::write(root.path().join("file"), b"data").unwrap();
        let outcome = load_children(root.path(), || true).unwrap();
        assert!(outcome.cancelled);
        assert!(outcome.entries.is_empty());
    }

    #[test]
    fn permission_errors_are_counted_without_panicking() {
        let mut warnings = ScanWarnings::default();
        let error = io::Error::new(io::ErrorKind::PermissionDenied, "protected");
        warnings.record_io(Path::new("/protected"), &error);

        assert_eq!(warnings.error_count, 1);
        assert_eq!(warnings.permission_denied, 1);
        assert_eq!(
            warnings.first_message.as_deref(),
            Some("Permission denied: /protected")
        );
    }
}
