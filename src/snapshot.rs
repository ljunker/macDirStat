use std::{
    collections::HashMap,
    fs::{self, File, OpenOptions},
    io::{BufReader, BufWriter, Write},
    path::{Path, PathBuf},
};

use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};

use crate::{
    analytics::{AnalysisIndex, AnalysisSummary},
    tree::UsageStats,
};

pub const SNAPSHOT_SCHEMA: u32 = 1;

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum ChangeKind {
    Added,
    Removed,
    Grown,
    Shrunk,
}

impl ChangeKind {
    pub fn label(self) -> &'static str {
        match self {
            Self::Added => "added",
            Self::Removed => "removed",
            Self::Grown => "grown",
            Self::Shrunk => "shrunk",
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum ChangeObject {
    File,
    Directory,
}

impl ChangeObject {
    pub fn label(self) -> &'static str {
        match self {
            Self::File => "file",
            Self::Directory => "directory",
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct ChangeRecord {
    #[serde(with = "crate::portable_path")]
    pub path: PathBuf,
    pub object: ChangeObject,
    pub kind: ChangeKind,
    pub previous: Option<UsageStats>,
    pub current: Option<UsageStats>,
    pub logical_delta: i128,
    pub physical_delta: i128,
    pub file_delta: i128,
}

#[derive(Clone, Debug, Default, Eq, PartialEq, Serialize, Deserialize)]
pub struct SnapshotComparison {
    pub changes: Vec<ChangeRecord>,
    pub added: usize,
    pub removed: usize,
    pub grown: usize,
    pub shrunk: usize,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct SnapshotV1 {
    pub schema_version: u32,
    pub app_version: String,
    pub index: AnalysisIndex,
    pub summary: AnalysisSummary,
    pub comparison: Option<SnapshotComparison>,
}

impl SnapshotV1 {
    pub fn from_index(index: &AnalysisIndex, comparison: Option<SnapshotComparison>) -> Self {
        Self {
            schema_version: SNAPSHOT_SCHEMA,
            app_version: env!("MACDIRSTAT_VERSION").to_owned(),
            summary: index.summary(index.files.iter()),
            index: index.clone(),
            comparison,
        }
    }

    fn validate(&self) -> Result<()> {
        if self.schema_version != SNAPSHOT_SCHEMA {
            bail!(
                "Unsupported snapshot schema {}; expected {}",
                self.schema_version,
                SNAPSHOT_SCHEMA
            );
        }
        Ok(())
    }
}

#[derive(Debug)]
pub struct RollingResult {
    pub comparison: Option<SnapshotComparison>,
    pub warning: Option<String>,
}

#[derive(Clone, Debug)]
pub struct SnapshotStore {
    directory: PathBuf,
    enabled: bool,
}

impl SnapshotStore {
    pub fn new(directory: PathBuf, enabled: bool) -> Self {
        Self { directory, enabled }
    }

    pub fn process_completed(&self, index: &AnalysisIndex) -> Result<RollingResult> {
        if !self.enabled || !index.complete {
            return Ok(RollingResult {
                comparison: None,
                warning: None,
            });
        }
        let path = self.rolling_path(index);
        let (baseline, warning) = if path.exists() {
            match load_snapshot(&path) {
                Ok(snapshot)
                    if snapshot.index.root == index.root
                        && snapshot.index.one_file_system == index.one_file_system =>
                {
                    (Some(snapshot), None)
                }
                Ok(_) => (
                    None,
                    Some("Ignoring snapshot for a different root or mount policy".to_owned()),
                ),
                Err(error) => (
                    None,
                    Some(format!("Ignoring unreadable rolling snapshot: {error}")),
                ),
            }
        } else {
            (None, None)
        };

        let mut current = SnapshotV1::from_index(index, None);
        let comparison = baseline.as_ref().map(|baseline| {
            merge_inaccessible(&mut current.index, &baseline.index);
            compare_indices(&baseline.index, &current.index)
        });
        current.summary = current.index.summary(current.index.files.iter());
        write_snapshot_atomic(&path, &current)?;
        Ok(RollingResult {
            comparison,
            warning,
        })
    }

    pub fn rolling_path(&self, index: &AnalysisIndex) -> PathBuf {
        let mut hasher = blake3::Hasher::new();
        hasher.update(path_bytes(&index.root));
        hasher.update(&[u8::from(index.one_file_system)]);
        self.directory
            .join(format!("{}.json", hasher.finalize().to_hex()))
    }
}

pub fn load_snapshot(path: &Path) -> Result<SnapshotV1> {
    let reader = BufReader::new(
        File::open(path).with_context(|| format!("Could not open {}", path.display()))?,
    );
    let snapshot: SnapshotV1 = serde_json::from_reader(reader)
        .with_context(|| format!("Invalid snapshot {}", path.display()))?;
    snapshot.validate()?;
    Ok(snapshot)
}

pub fn load_compatible_snapshot(path: &Path, index: &AnalysisIndex) -> Result<SnapshotV1> {
    let snapshot = load_snapshot(path)?;
    if snapshot.index.root != index.root {
        bail!("Snapshot root does not match {}", index.root.display());
    }
    if snapshot.index.one_file_system != index.one_file_system {
        bail!("Snapshot mount policy does not match the current scan");
    }
    Ok(snapshot)
}

pub fn write_export(
    path: &Path,
    index: &AnalysisIndex,
    comparison: Option<SnapshotComparison>,
) -> Result<()> {
    let snapshot = SnapshotV1::from_index(index, comparison);
    if path.as_os_str() == "-" {
        let stdout = std::io::stdout();
        let mut writer = BufWriter::new(stdout.lock());
        serde_json::to_writer_pretty(&mut writer, &snapshot)?;
        writer.write_all(b"\n")?;
        writer.flush()?;
        Ok(())
    } else {
        write_snapshot_atomic(path, &snapshot)
    }
}

pub fn compare_indices(previous: &AnalysisIndex, current: &AnalysisIndex) -> SnapshotComparison {
    let mut old = records(previous);
    let mut new = records(current);
    let inaccessible: Vec<_> = current
        .issues
        .iter()
        .map(|issue| issue.path.as_path())
        .collect();
    let mut comparison = SnapshotComparison::default();

    for (key, previous_usage) in old.drain() {
        match new.remove(&key) {
            Some(current_usage) if current_usage != previous_usage => {
                let record = change_record(key, Some(previous_usage), Some(current_usage));
                count_change(&mut comparison, record.kind);
                comparison.changes.push(record);
            }
            Some(_) => {}
            None if inaccessible.iter().any(|prefix| key.1.starts_with(prefix)) => {}
            None => {
                let record = change_record(key, Some(previous_usage), None);
                count_change(&mut comparison, record.kind);
                comparison.changes.push(record);
            }
        }
    }
    for (key, current_usage) in new {
        let record = change_record(key, None, Some(current_usage));
        count_change(&mut comparison, record.kind);
        comparison.changes.push(record);
    }
    comparison.changes.sort_by_key(|record| {
        std::cmp::Reverse(
            record
                .logical_delta
                .unsigned_abs()
                .max(record.physical_delta.unsigned_abs()),
        )
    });
    comparison
}

fn records(index: &AnalysisIndex) -> HashMap<(ChangeObject, PathBuf), UsageStats> {
    let files = index
        .files
        .iter()
        .map(|file| ((ChangeObject::File, file.path.clone()), file.usage));
    let directories = index.directories.iter().map(|directory| {
        (
            (ChangeObject::Directory, directory.path.clone()),
            directory.usage,
        )
    });
    files.chain(directories).collect()
}

fn change_record(
    (object, path): (ChangeObject, PathBuf),
    previous: Option<UsageStats>,
    current: Option<UsageStats>,
) -> ChangeRecord {
    let old = previous.unwrap_or_default();
    let new = current.unwrap_or_default();
    let logical_delta = i128::from(new.logical) - i128::from(old.logical);
    let physical_delta = i128::from(new.physical) - i128::from(old.physical);
    let file_delta = i128::from(new.files) - i128::from(old.files);
    let kind = match (previous, current) {
        (None, Some(_)) => ChangeKind::Added,
        (Some(_), None) => ChangeKind::Removed,
        (Some(_), Some(_))
            if logical_delta > 0
                || logical_delta == 0 && physical_delta > 0
                || logical_delta == 0 && physical_delta == 0 && file_delta > 0 =>
        {
            ChangeKind::Grown
        }
        (Some(_), Some(_)) => ChangeKind::Shrunk,
        (None, None) => unreachable!("a change needs at least one side"),
    };
    ChangeRecord {
        path,
        object,
        kind,
        previous,
        current,
        logical_delta,
        physical_delta,
        file_delta,
    }
}

fn count_change(comparison: &mut SnapshotComparison, kind: ChangeKind) {
    match kind {
        ChangeKind::Added => comparison.added += 1,
        ChangeKind::Removed => comparison.removed += 1,
        ChangeKind::Grown => comparison.grown += 1,
        ChangeKind::Shrunk => comparison.shrunk += 1,
    }
}

fn merge_inaccessible(current: &mut AnalysisIndex, previous: &AnalysisIndex) {
    let inaccessible: Vec<_> = current
        .issues
        .iter()
        .map(|issue| issue.path.as_path())
        .collect();
    for file in &previous.files {
        if inaccessible
            .iter()
            .any(|prefix| file.path.starts_with(prefix))
            && !current
                .files
                .iter()
                .any(|current| current.path == file.path)
        {
            current.files.push(file.clone());
        }
    }
    for directory in &previous.directories {
        if inaccessible
            .iter()
            .any(|prefix| directory.path.starts_with(prefix))
            && !current
                .directories
                .iter()
                .any(|current| current.path == directory.path)
        {
            current.directories.push(directory.clone());
        }
    }
}

fn write_snapshot_atomic(path: &Path, snapshot: &SnapshotV1) -> Result<()> {
    let parent = path
        .parent()
        .filter(|parent| !parent.as_os_str().is_empty())
        .unwrap_or_else(|| Path::new("."));
    fs::create_dir_all(parent).with_context(|| format!("Could not create {}", parent.display()))?;
    let temp_path = path.with_extension(format!("json.tmp-{}", std::process::id()));
    let result = (|| -> Result<()> {
        let file = OpenOptions::new()
            .create(true)
            .truncate(true)
            .write(true)
            .open(&temp_path)
            .with_context(|| format!("Could not create {}", temp_path.display()))?;
        let mut writer = BufWriter::new(file);
        serde_json::to_writer_pretty(&mut writer, snapshot)?;
        writer.flush()?;
        fs::rename(&temp_path, path)
            .with_context(|| format!("Could not replace {}", path.display()))?;
        Ok(())
    })();
    if result.is_err() {
        let _ = fs::remove_file(&temp_path);
    }
    result
}

#[cfg(unix)]
fn path_bytes(path: &Path) -> &[u8] {
    use std::os::unix::ffi::OsStrExt;
    path.as_os_str().as_bytes()
}

#[cfg(not(unix))]
fn path_bytes(path: &Path) -> &[u8] {
    path.to_str().unwrap_or_default().as_bytes()
}

#[cfg(test)]
mod tests {
    use std::ffi::OsString;

    #[cfg(unix)]
    use std::os::unix::ffi::OsStringExt;
    use tempfile::tempdir;

    use super::*;
    use crate::{
        analytics::{DirectoryRecord, FileCategory, FileRecord},
        tree::FileIdentity,
    };

    fn identity(inode: u64) -> FileIdentity {
        FileIdentity {
            device: 1,
            inode,
            modified_seconds: 1,
            modified_nanoseconds: 0,
        }
    }

    fn index(root: &Path, file_name: &str, size: u64) -> AnalysisIndex {
        let path = root.join(file_name);
        let usage = UsageStats {
            logical: size,
            physical: size,
            files: 1,
        };
        AnalysisIndex {
            root: root.to_path_buf(),
            one_file_system: true,
            started_at: 1,
            completed_at: Some(2),
            files: vec![FileRecord {
                path: path.clone(),
                usage,
                identity: identity(2),
                modified_seconds: 1,
                modified_nanoseconds: 0,
                extension: None,
                category: FileCategory::Other,
            }],
            directories: vec![DirectoryRecord {
                path: root.to_path_buf(),
                usage,
                identity: identity(1),
                modified_seconds: 1,
                modified_nanoseconds: 0,
            }],
            issues: Vec::new(),
            mounts_skipped: 0,
            complete: true,
            duplicates: None,
        }
    }

    #[test]
    fn comparison_reports_added_removed_grown_and_shrunk() {
        let root = Path::new("/tmp/root");
        let mut previous = index(root, "grown", 10);
        previous.files.push(FileRecord {
            path: root.join("removed"),
            usage: UsageStats {
                logical: 4,
                physical: 4,
                files: 1,
            },
            identity: identity(3),
            modified_seconds: 1,
            modified_nanoseconds: 0,
            extension: None,
            category: FileCategory::Other,
        });
        let mut current = index(root, "grown", 20);
        current.files.push(FileRecord {
            path: root.join("added"),
            usage: UsageStats {
                logical: 5,
                physical: 5,
                files: 1,
            },
            identity: identity(4),
            modified_seconds: 1,
            modified_nanoseconds: 0,
            extension: None,
            category: FileCategory::Other,
        });
        let comparison = compare_indices(&previous, &current);
        assert!(
            comparison.changes.iter().any(|change| {
                change.path.ends_with("grown") && change.kind == ChangeKind::Grown
            })
        );
        assert!(comparison.changes.iter().any(|change| {
            change.path.ends_with("removed") && change.kind == ChangeKind::Removed
        }));
        assert!(
            comparison.changes.iter().any(|change| {
                change.path.ends_with("added") && change.kind == ChangeKind::Added
            })
        );
    }

    #[test]
    fn rolling_snapshot_is_atomic_and_compares_previous_scan() {
        let directory = tempdir().unwrap();
        let root = directory.path().join("root");
        let store = SnapshotStore::new(directory.path().join("snapshots"), true);
        let first = index(&root, "file", 10);
        assert!(
            store
                .process_completed(&first)
                .unwrap()
                .comparison
                .is_none()
        );
        let second = index(&root, "file", 20);
        let result = store.process_completed(&second).unwrap();
        assert_eq!(result.comparison.unwrap().grown, 2);
        let path = store.rolling_path(&second);
        assert!(path.exists());
        assert!(
            !path
                .with_extension(format!("json.tmp-{}", std::process::id()))
                .exists()
        );
    }

    #[test]
    fn incomplete_scan_does_not_replace_rolling_baseline() {
        let directory = tempdir().unwrap();
        let root = directory.path().join("root");
        let store = SnapshotStore::new(directory.path().join("snapshots"), true);
        let baseline = index(&root, "file", 10);
        store.process_completed(&baseline).unwrap();
        let path = store.rolling_path(&baseline);
        let before = fs::read(&path).unwrap();

        let mut incomplete = index(&root, "file", 99);
        incomplete.complete = false;
        incomplete.completed_at = None;
        assert!(
            store
                .process_completed(&incomplete)
                .unwrap()
                .comparison
                .is_none()
        );
        assert_eq!(fs::read(path).unwrap(), before);
    }

    #[test]
    fn rejects_unknown_snapshot_schema() {
        let directory = tempdir().unwrap();
        let path = directory.path().join("future.json");
        let snapshot = SnapshotV1::from_index(&index(directory.path(), "file", 1), None);
        let mut value = serde_json::to_value(snapshot).unwrap();
        value["schema_version"] = serde_json::json!(999);
        fs::write(&path, serde_json::to_vec(&value).unwrap()).unwrap();
        assert!(load_snapshot(&path).is_err());
    }

    #[cfg(unix)]
    #[test]
    fn snapshot_roundtrip_preserves_non_utf8_paths() {
        let directory = tempdir().unwrap();
        let root = PathBuf::from(OsString::from_vec(vec![b'/', b't', b'm', b'p', b'/', 0xff]));
        let value = index(&root, "file", 10);
        let path = directory.path().join("snapshot.json");
        write_export(&path, &value, None).unwrap();
        let loaded = load_snapshot(&path).unwrap();
        assert_eq!(loaded.index.root, root);
    }
}
