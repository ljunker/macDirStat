use std::{
    ffi::OsString,
    fs, io,
    path::{Path, PathBuf},
    time::SystemTime,
};

#[cfg(unix)]
use std::os::unix::fs::MetadataExt;

use walkdir::WalkDir;

use crate::tree::{FileIdentity, NodeKind, UsageStats};

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct ScanOptions {
    pub root_device: u64,
    pub one_file_system: bool,
}

#[derive(Debug)]
pub struct DiscoveredEntry {
    pub path: PathBuf,
    pub name: OsString,
    pub kind: NodeKind,
    pub usage: Option<UsageStats>,
    pub error: Option<String>,
    pub identity: Option<FileIdentity>,
    pub modified: Option<SystemTime>,
    pub mountpoint: bool,
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
    pub usage: UsageStats,
    pub warnings: ScanWarnings,
    pub fatal_error: Option<String>,
    pub cancelled: bool,
    pub mounts_skipped: usize,
}

pub fn root_device(path: &Path) -> io::Result<u64> {
    fs::metadata(path).map(|metadata| device(&metadata))
}

pub fn load_children<F>(
    path: &Path,
    options: ScanOptions,
    is_cancelled: F,
) -> io::Result<LoadOutcome>
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
                let identity = file_identity(&metadata);
                let mountpoint = is_mountpoint(kind, identity, options);
                let usage = match kind {
                    NodeKind::Directory => None,
                    NodeKind::File => Some(UsageStats {
                        logical: metadata.len(),
                        physical: physical_size(&metadata),
                        files: 1,
                    }),
                    NodeKind::Symlink | NodeKind::Other => Some(UsageStats {
                        logical: metadata.len(),
                        physical: physical_size(&metadata),
                        files: 0,
                    }),
                };
                entries.push(DiscoveredEntry {
                    path: entry_path,
                    name,
                    kind,
                    usage,
                    error: None,
                    identity: Some(identity),
                    modified: metadata.modified().ok(),
                    mountpoint,
                });
            }
            Err(error) => {
                warnings.record_io(&entry_path, &error);
                entries.push(DiscoveredEntry {
                    path: entry_path,
                    name,
                    kind: NodeKind::Other,
                    usage: None,
                    error: Some(error.to_string()),
                    identity: None,
                    modified: None,
                    mountpoint: false,
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

pub fn calculate_size<F>(path: &Path, options: ScanOptions, is_cancelled: F) -> ScanOutcome
where
    F: Fn() -> bool,
{
    let mut usage = UsageStats::default();
    let mut warnings = ScanWarnings::default();
    let mut fatal_error = None;
    let mut mounts_skipped = 0;
    let mut walker = WalkDir::new(path).follow_links(false).into_iter();

    while let Some(item) = walker.next() {
        if is_cancelled() {
            return ScanOutcome {
                usage,
                warnings,
                fatal_error,
                cancelled: true,
                mounts_skipped,
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

        let metadata = match fs::symlink_metadata(entry.path()) {
            Ok(metadata) => metadata,
            Err(error) => {
                warnings.record_io(entry.path(), &error);
                continue;
            }
        };
        let different_device = options.one_file_system && device(&metadata) != options.root_device;
        if different_device {
            if entry.file_type().is_dir() {
                walker.skip_current_dir();
                mounts_skipped += 1;
            }
            continue;
        }

        if entry.depth() == 0 || entry.file_type().is_dir() {
            continue;
        }
        if entry.file_type().is_file() {
            usage.logical = usage.logical.saturating_add(metadata.len());
            usage.physical = usage.physical.saturating_add(physical_size(&metadata));
            usage.files = usage.files.saturating_add(1);
        } else if entry.file_type().is_symlink() {
            usage.logical = usage.logical.saturating_add(metadata.len());
            usage.physical = usage.physical.saturating_add(physical_size(&metadata));
        }
    }

    ScanOutcome {
        usage,
        warnings,
        fatal_error,
        cancelled: false,
        mounts_skipped,
    }
}

#[cfg(unix)]
fn file_identity(metadata: &fs::Metadata) -> FileIdentity {
    FileIdentity {
        device: metadata.dev(),
        inode: metadata.ino(),
        modified_seconds: metadata.mtime(),
        modified_nanoseconds: metadata.mtime_nsec(),
    }
}

#[cfg(not(unix))]
fn file_identity(metadata: &fs::Metadata) -> FileIdentity {
    let modified = metadata
        .modified()
        .ok()
        .and_then(|time| time.duration_since(SystemTime::UNIX_EPOCH).ok());
    FileIdentity {
        device: 0,
        inode: 0,
        modified_seconds: modified.map_or(0, |value| value.as_secs() as i64),
        modified_nanoseconds: modified.map_or(0, |value| value.subsec_nanos() as i64),
    }
}

#[cfg(unix)]
fn device(metadata: &fs::Metadata) -> u64 {
    metadata.dev()
}

#[cfg(not(unix))]
fn device(_metadata: &fs::Metadata) -> u64 {
    0
}

#[cfg(unix)]
fn physical_size(metadata: &fs::Metadata) -> u64 {
    metadata.blocks().saturating_mul(512)
}

#[cfg(not(unix))]
fn physical_size(metadata: &fs::Metadata) -> u64 {
    metadata.len()
}

fn is_mountpoint(kind: NodeKind, identity: FileIdentity, options: ScanOptions) -> bool {
    kind == NodeKind::Directory && options.one_file_system && identity.device != options.root_device
}

#[cfg(test)]
mod tests {
    use std::fs;

    use tempfile::tempdir;

    use super::*;

    fn options(path: &Path) -> ScanOptions {
        ScanOptions {
            root_device: root_device(path).unwrap(),
            one_file_system: true,
        }
    }

    #[test]
    fn calculates_both_sizes_and_regular_file_count() {
        let root = tempdir().unwrap();
        let nested = root.path().join("nested");
        fs::create_dir(&nested).unwrap();
        fs::write(root.path().join("one"), vec![0_u8; 10]).unwrap();
        fs::write(nested.join("two"), vec![0_u8; 25]).unwrap();

        let outcome = calculate_size(root.path(), options(root.path()), || false);
        assert_eq!(outcome.usage.logical, 35);
        assert!(outcome.usage.physical >= outcome.usage.logical);
        assert_eq!(outcome.usage.files, 2);
        assert_eq!(outcome.warnings.error_count, 0);
        assert!(!outcome.cancelled);
    }

    #[cfg(unix)]
    #[test]
    fn sparse_file_reports_less_physical_than_logical_space() {
        let root = tempdir().unwrap();
        let sparse = fs::File::create(root.path().join("sparse")).unwrap();
        sparse.set_len(8 * 1024 * 1024).unwrap();

        let outcome = calculate_size(root.path(), options(root.path()), || false);
        assert_eq!(outcome.usage.logical, 8 * 1024 * 1024);
        assert!(outcome.usage.physical < outcome.usage.logical);
        assert_eq!(outcome.usage.files, 1);
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

        let outcome = calculate_size(&scan_root, options(&scan_root), || false);
        assert_eq!(
            outcome.usage.logical,
            fs::symlink_metadata(link).unwrap().len()
        );
        assert_eq!(outcome.usage.files, 0);
    }

    #[test]
    fn missing_root_becomes_a_fatal_scan_error() {
        let root = tempdir().unwrap();
        let missing = root.path().join("gone");
        let outcome = calculate_size(
            &missing,
            ScanOptions {
                root_device: 0,
                one_file_system: true,
            },
            || false,
        );
        assert!(outcome.fatal_error.is_some());
        assert!(outcome.warnings.error_count > 0);
    }

    #[test]
    fn cancellation_stops_directory_loading_and_size_scan() {
        let root = tempdir().unwrap();
        fs::write(root.path().join("file"), b"data").unwrap();
        let scan_options = options(root.path());
        let outcome = load_children(root.path(), scan_options, || true).unwrap();
        assert!(outcome.cancelled);
        assert!(outcome.entries.is_empty());

        let outcome = calculate_size(root.path(), scan_options, || true);
        assert!(outcome.cancelled);
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

    #[test]
    fn mountpoint_detection_respects_scan_policy() {
        let identity = FileIdentity {
            device: 2,
            inode: 1,
            modified_seconds: 0,
            modified_nanoseconds: 0,
        };
        assert!(is_mountpoint(
            NodeKind::Directory,
            identity,
            ScanOptions {
                root_device: 1,
                one_file_system: true,
            }
        ));
        assert!(!is_mountpoint(
            NodeKind::Directory,
            identity,
            ScanOptions {
                root_device: 1,
                one_file_system: false,
            }
        ));
        assert!(!is_mountpoint(
            NodeKind::File,
            identity,
            ScanOptions {
                root_device: 1,
                one_file_system: true,
            }
        ));
    }

    #[test]
    fn size_scan_stops_at_a_device_boundary() {
        let root = tempdir().unwrap();
        fs::write(root.path().join("file"), b"data").unwrap();
        let actual_device = root_device(root.path()).unwrap();

        let stopped = calculate_size(
            root.path(),
            ScanOptions {
                root_device: actual_device.wrapping_add(1),
                one_file_system: true,
            },
            || false,
        );
        assert_eq!(stopped.usage, UsageStats::default());
        assert_eq!(stopped.mounts_skipped, 1);

        let crossed = calculate_size(
            root.path(),
            ScanOptions {
                root_device: actual_device.wrapping_add(1),
                one_file_system: false,
            },
            || false,
        );
        assert_eq!(crossed.usage.files, 1);
        assert_eq!(crossed.usage.logical, 4);
    }
}
