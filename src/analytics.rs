use std::{
    collections::{BTreeMap, HashMap},
    fs::{self, File},
    io::{Read, Seek, SeekFrom},
    path::{Path, PathBuf},
    sync::{
        Arc,
        atomic::{AtomicU64, Ordering},
    },
    thread::{self, JoinHandle},
    time::{SystemTime, UNIX_EPOCH},
};

use anyhow::{Context, Result};
use crossbeam_channel::{Receiver, Sender, TryRecvError, unbounded};
use serde::{Deserialize, Serialize};
use walkdir::WalkDir;

#[cfg(unix)]
use std::os::unix::fs::MetadataExt;

use crate::{
    scanner::ScanOptions,
    tree::{FileIdentity, UsageStats},
};

const FILE_BATCH_SIZE: usize = 512;
const HASH_BUFFER_SIZE: usize = 256 * 1024;
const SAMPLE_SIZE: usize = 64 * 1024;
const MAX_ISSUES: usize = 10_000;

#[derive(Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum FileCategory {
    Image,
    Video,
    Audio,
    Archive,
    Document,
    Code,
    Other,
}

impl FileCategory {
    pub const ALL: [Self; 7] = [
        Self::Image,
        Self::Video,
        Self::Audio,
        Self::Archive,
        Self::Document,
        Self::Code,
        Self::Other,
    ];

    pub fn label(self) -> &'static str {
        match self {
            Self::Image => "image",
            Self::Video => "video",
            Self::Audio => "audio",
            Self::Archive => "archive",
            Self::Document => "document",
            Self::Code => "code",
            Self::Other => "other",
        }
    }

    pub fn parse(value: &str) -> Option<Self> {
        Self::ALL
            .into_iter()
            .find(|category| category.label().eq_ignore_ascii_case(value))
    }
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct FileRecord {
    #[serde(with = "crate::portable_path")]
    pub path: PathBuf,
    pub usage: UsageStats,
    pub identity: FileIdentity,
    pub modified_seconds: i64,
    pub modified_nanoseconds: i64,
    pub extension: Option<String>,
    pub category: FileCategory,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct DirectoryRecord {
    #[serde(with = "crate::portable_path")]
    pub path: PathBuf,
    pub usage: UsageStats,
    pub identity: FileIdentity,
    pub modified_seconds: i64,
    pub modified_nanoseconds: i64,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct AnalysisIssue {
    #[serde(with = "crate::portable_path")]
    pub path: PathBuf,
    pub message: String,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct DuplicateEntry {
    #[serde(with = "crate::portable_path")]
    pub path: PathBuf,
    pub usage: UsageStats,
    pub identity: FileIdentity,
    #[serde(with = "crate::portable_path::vec")]
    pub hardlink_aliases: Vec<PathBuf>,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct DuplicateGroup {
    pub hash: String,
    pub logical_size: u64,
    pub reclaimable_logical: u64,
    pub physical_total: u64,
    pub files: Vec<DuplicateEntry>,
}

#[derive(Clone, Debug, Default, Eq, PartialEq, Serialize, Deserialize)]
pub struct GroupStats {
    pub files: u64,
    pub usage: UsageStats,
}

#[derive(Clone, Debug, Default, Eq, PartialEq, Serialize, Deserialize)]
pub struct AnalysisSummary {
    pub total: UsageStats,
    pub categories: BTreeMap<String, GroupStats>,
    pub extensions: BTreeMap<String, GroupStats>,
    pub age_buckets: BTreeMap<String, GroupStats>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct AnalysisIndex {
    #[serde(with = "crate::portable_path")]
    pub root: PathBuf,
    pub one_file_system: bool,
    pub started_at: u64,
    pub completed_at: Option<u64>,
    pub files: Vec<FileRecord>,
    pub directories: Vec<DirectoryRecord>,
    pub issues: Vec<AnalysisIssue>,
    pub mounts_skipped: usize,
    pub complete: bool,
    pub duplicates: Option<Vec<DuplicateGroup>>,
}

impl AnalysisIndex {
    pub fn new(root: PathBuf, one_file_system: bool, started_at: u64) -> Self {
        Self {
            root,
            one_file_system,
            started_at,
            completed_at: None,
            files: Vec::new(),
            directories: Vec::new(),
            issues: Vec::new(),
            mounts_skipped: 0,
            complete: false,
            duplicates: None,
        }
    }

    pub fn root_usage(&self) -> UsageStats {
        self.directories
            .iter()
            .find(|directory| directory.path == self.root)
            .map_or_else(
                || {
                    self.files
                        .iter()
                        .map(|file| file.usage)
                        .fold(UsageStats::default(), UsageStats::saturating_add)
                },
                |directory| directory.usage,
            )
    }

    pub fn summary<'a>(&'a self, files: impl Iterator<Item = &'a FileRecord>) -> AnalysisSummary {
        let now = now_seconds();
        let mut summary = AnalysisSummary::default();
        for file in files {
            summary.total = summary.total.saturating_add(file.usage);
            add_group(
                &mut summary.categories,
                file.category.label().to_owned(),
                file.usage,
            );
            add_group(
                &mut summary.extensions,
                file.extension
                    .clone()
                    .unwrap_or_else(|| "<none>".to_owned()),
                file.usage,
            );
            let age = if file.modified_seconds < 0 {
                "unknown"
            } else {
                let seconds = now.saturating_sub(file.modified_seconds as u64);
                match seconds {
                    0..=86_399 => "<24h",
                    86_400..=604_799 => "1-7d",
                    604_800..=2_591_999 => "7-30d",
                    2_592_000..=31_535_999 => "30-365d",
                    _ => ">1y",
                }
            };
            add_group(&mut summary.age_buckets, age.to_owned(), file.usage);
        }
        summary
    }

    pub fn remove_subtree(&mut self, path: &Path) {
        self.files.retain(|file| !file.path.starts_with(path));
        self.directories
            .retain(|directory| !directory.path.starts_with(path));
        self.duplicates = None;
    }
}

fn add_group(groups: &mut BTreeMap<String, GroupStats>, key: String, usage: UsageStats) {
    let group = groups.entry(key).or_default();
    group.files = group.files.saturating_add(1);
    group.usage = group.usage.saturating_add(usage);
}

#[derive(Debug)]
pub enum AnalysisEvent {
    Started {
        generation: u64,
        scan_root: PathBuf,
        started_at: u64,
    },
    FileBatch {
        generation: u64,
        files: Vec<FileRecord>,
    },
    Finished {
        generation: u64,
        scan_root: PathBuf,
        completed_at: u64,
        directories: Vec<DirectoryRecord>,
        issues: Vec<AnalysisIssue>,
        mounts_skipped: usize,
    },
    DuplicatesStarted {
        generation: u64,
        candidates: usize,
    },
    DuplicatesFinished {
        generation: u64,
        groups: Vec<DuplicateGroup>,
        issues: Vec<AnalysisIssue>,
    },
}

enum AnalysisRequest {
    Index {
        generation: u64,
        root: PathBuf,
        options: ScanOptions,
    },
    Duplicates {
        generation: u64,
        files: Vec<FileRecord>,
    },
}

pub struct AnalysisWorker {
    request_tx: Option<Sender<AnalysisRequest>>,
    event_rx: Receiver<AnalysisEvent>,
    active_generation: Arc<AtomicU64>,
    handle: Option<JoinHandle<()>>,
}

impl AnalysisWorker {
    pub fn new(generation: u64) -> Result<Self> {
        let (request_tx, request_rx) = unbounded();
        let (event_tx, event_rx) = unbounded();
        let active_generation = Arc::new(AtomicU64::new(generation));
        let active = Arc::clone(&active_generation);
        let handle = thread::Builder::new()
            .name("macDirStat-analysis".to_owned())
            .spawn(move || analysis_loop(request_rx, event_tx, active))
            .context("Could not start analysis worker")?;
        Ok(Self {
            request_tx: Some(request_tx),
            event_rx,
            active_generation,
            handle: Some(handle),
        })
    }

    pub fn start_index(&self, generation: u64, root: PathBuf, options: ScanOptions) -> Result<()> {
        self.active_generation.store(generation, Ordering::Release);
        self.request_tx
            .as_ref()
            .context("Analysis worker is shut down")?
            .send(AnalysisRequest::Index {
                generation,
                root,
                options,
            })
            .context("Analysis worker stopped unexpectedly")
    }

    pub fn start_duplicates(&self, generation: u64, files: Vec<FileRecord>) -> Result<()> {
        self.active_generation.store(generation, Ordering::Release);
        self.request_tx
            .as_ref()
            .context("Analysis worker is shut down")?
            .send(AnalysisRequest::Duplicates { generation, files })
            .context("Analysis worker stopped unexpectedly")
    }

    pub fn set_generation(&self, generation: u64) {
        self.active_generation.store(generation, Ordering::Release);
    }

    pub fn try_recv(&self) -> Result<AnalysisEvent, TryRecvError> {
        self.event_rx.try_recv()
    }

    pub fn discard_pending(&self) -> usize {
        let mut discarded = 0;
        while self.event_rx.try_recv().is_ok() {
            discarded += 1;
        }
        discarded
    }
}

impl Drop for AnalysisWorker {
    fn drop(&mut self) {
        self.active_generation.store(u64::MAX, Ordering::Release);
        self.request_tx.take();
        if let Some(handle) = self.handle.take() {
            let _ = handle.join();
        }
    }
}

pub fn build_index(root: PathBuf, options: ScanOptions, hash_duplicates: bool) -> AnalysisIndex {
    let generation = 1;
    let active_generation = AtomicU64::new(generation);
    let (event_tx, event_rx) = unbounded();
    let started_at = now_seconds();
    scan_index(generation, &root, options, &event_tx, &active_generation);
    drop(event_tx);

    let mut index = AnalysisIndex::new(root, options.one_file_system, started_at);
    for event in event_rx {
        match event {
            AnalysisEvent::FileBatch { files, .. } => index.files.extend(files),
            AnalysisEvent::Finished {
                completed_at,
                directories,
                issues,
                mounts_skipped,
                ..
            } => {
                index.completed_at = Some(completed_at);
                index.directories = directories;
                index.issues = issues;
                index.mounts_skipped = mounts_skipped;
                index.complete = true;
            }
            _ => {}
        }
    }
    if hash_duplicates {
        let (groups, issues, _) = detect_duplicates(&index.files, generation, &active_generation);
        index.duplicates = Some(groups);
        index.issues.extend(issues);
    }
    index
}

fn analysis_loop(
    requests: Receiver<AnalysisRequest>,
    events: Sender<AnalysisEvent>,
    active_generation: Arc<AtomicU64>,
) {
    for request in requests {
        match request {
            AnalysisRequest::Index {
                generation,
                root,
                options,
            } => {
                if active_generation.load(Ordering::Acquire) != generation {
                    continue;
                }
                let started_at = now_seconds();
                if events
                    .send(AnalysisEvent::Started {
                        generation,
                        scan_root: root.clone(),
                        started_at,
                    })
                    .is_err()
                {
                    break;
                }
                scan_index(generation, &root, options, &events, &active_generation);
            }
            AnalysisRequest::Duplicates { generation, files } => {
                if active_generation.load(Ordering::Acquire) != generation {
                    continue;
                }
                let candidates = duplicate_candidate_count(&files);
                if events
                    .send(AnalysisEvent::DuplicatesStarted {
                        generation,
                        candidates,
                    })
                    .is_err()
                {
                    break;
                }
                let (groups, issues, cancelled) =
                    detect_duplicates(&files, generation, &active_generation);
                if !cancelled
                    && events
                        .send(AnalysisEvent::DuplicatesFinished {
                            generation,
                            groups,
                            issues,
                        })
                        .is_err()
                {
                    break;
                }
            }
        }
    }
}

fn scan_index(
    generation: u64,
    root: &Path,
    options: ScanOptions,
    events: &Sender<AnalysisEvent>,
    active_generation: &AtomicU64,
) {
    let mut batch = Vec::with_capacity(FILE_BATCH_SIZE);
    let mut directories = HashMap::<PathBuf, DirectoryRecord>::new();
    let mut issues = Vec::new();
    let mut mounts_skipped = 0;
    let mut walker = WalkDir::new(root).follow_links(false).into_iter();

    while let Some(item) = walker.next() {
        if active_generation.load(Ordering::Relaxed) != generation {
            return;
        }
        let entry = match item {
            Ok(entry) => entry,
            Err(error) => {
                push_issue(&mut issues, error.path().unwrap_or(root), error.to_string());
                continue;
            }
        };
        let metadata = match fs::symlink_metadata(entry.path()) {
            Ok(metadata) => metadata,
            Err(error) => {
                push_issue(&mut issues, entry.path(), error.to_string());
                continue;
            }
        };
        if options.one_file_system && metadata_device(&metadata) != options.root_device {
            if entry.file_type().is_dir() {
                walker.skip_current_dir();
                mounts_skipped += 1;
            }
            continue;
        }

        if entry.file_type().is_dir() {
            let identity = metadata_identity(&metadata);
            let (modified_seconds, modified_nanoseconds) = metadata_modified(&metadata);
            directories
                .entry(entry.path().to_path_buf())
                .or_insert(DirectoryRecord {
                    path: entry.path().to_path_buf(),
                    usage: UsageStats::default(),
                    identity,
                    modified_seconds,
                    modified_nanoseconds,
                });
            continue;
        }

        let usage = UsageStats {
            logical: metadata.len(),
            physical: metadata_physical_size(&metadata),
            files: u64::from(entry.file_type().is_file()),
        };
        if let Some(parent) = entry.path().parent()
            && let Some(directory) = directories.get_mut(parent)
        {
            directory.usage = directory.usage.saturating_add(usage);
        }
        if !entry.file_type().is_file() {
            continue;
        }

        let extension = normalized_extension(entry.path());
        let identity = metadata_identity(&metadata);
        let (modified_seconds, modified_nanoseconds) = metadata_modified(&metadata);
        batch.push(FileRecord {
            path: entry.path().to_path_buf(),
            usage,
            identity,
            modified_seconds,
            modified_nanoseconds,
            category: category_for_extension(extension.as_deref()),
            extension,
        });
        if batch.len() >= FILE_BATCH_SIZE {
            if events
                .send(AnalysisEvent::FileBatch {
                    generation,
                    files: std::mem::take(&mut batch),
                })
                .is_err()
            {
                return;
            }
            batch = Vec::with_capacity(FILE_BATCH_SIZE);
        }
    }

    if !batch.is_empty()
        && events
            .send(AnalysisEvent::FileBatch {
                generation,
                files: batch,
            })
            .is_err()
    {
        return;
    }

    let mut paths: Vec<_> = directories.keys().cloned().collect();
    paths.sort_by_key(|path| std::cmp::Reverse(path.components().count()));
    for path in paths {
        if path == root {
            continue;
        }
        let Some(usage) = directories.get(&path).map(|directory| directory.usage) else {
            continue;
        };
        if let Some(parent) = path.parent()
            && let Some(parent_record) = directories.get_mut(parent)
        {
            parent_record.usage = parent_record.usage.saturating_add(usage);
        }
    }
    let mut directories: Vec<_> = directories.into_values().collect();
    directories.sort_by(|left, right| left.path.cmp(&right.path));
    let _ = events.send(AnalysisEvent::Finished {
        generation,
        scan_root: root.to_path_buf(),
        completed_at: now_seconds(),
        directories,
        issues,
        mounts_skipped,
    });
}

fn duplicate_candidate_count(files: &[FileRecord]) -> usize {
    let mut sizes = HashMap::<u64, usize>::new();
    for file in files.iter().filter(|file| file.usage.logical > 0) {
        *sizes.entry(file.usage.logical).or_default() += 1;
    }
    files
        .iter()
        .filter(|file| sizes.get(&file.usage.logical).copied().unwrap_or_default() > 1)
        .count()
}

fn detect_duplicates(
    files: &[FileRecord],
    generation: u64,
    active_generation: &AtomicU64,
) -> (Vec<DuplicateGroup>, Vec<AnalysisIssue>, bool) {
    let mut by_size = HashMap::<u64, Vec<&FileRecord>>::new();
    for file in files.iter().filter(|file| file.usage.logical > 0) {
        by_size.entry(file.usage.logical).or_default().push(file);
    }
    let mut issues = Vec::new();
    let mut groups = Vec::new();

    for (logical_size, candidates) in by_size
        .into_iter()
        .filter(|(_, candidates)| candidates.len() > 1)
    {
        if active_generation.load(Ordering::Relaxed) != generation {
            return (groups, issues, true);
        }
        let mut identities = HashMap::<(u64, u64), (&FileRecord, Vec<PathBuf>)>::new();
        for candidate in candidates {
            identities
                .entry((candidate.identity.device, candidate.identity.inode))
                .and_modify(|(_, aliases)| aliases.push(candidate.path.clone()))
                .or_insert((candidate, Vec::new()));
        }
        if identities.len() < 2 {
            continue;
        }

        let mut by_sample = HashMap::<blake3::Hash, Vec<(&FileRecord, Vec<PathBuf>)>>::new();
        for (_, (candidate, aliases)) in identities {
            match sample_hash(&candidate.path) {
                Ok(hash) => by_sample
                    .entry(hash)
                    .or_default()
                    .push((candidate, aliases)),
                Err(error) => push_issue(&mut issues, &candidate.path, error.to_string()),
            }
        }
        for sampled in by_sample.into_values().filter(|sampled| sampled.len() > 1) {
            let mut by_hash = HashMap::<blake3::Hash, Vec<DuplicateEntry>>::new();
            for (candidate, aliases) in sampled {
                if active_generation.load(Ordering::Relaxed) != generation {
                    return (groups, issues, true);
                }
                match full_hash(candidate, generation, active_generation) {
                    Ok(Some(hash)) => by_hash.entry(hash).or_default().push(DuplicateEntry {
                        path: candidate.path.clone(),
                        usage: candidate.usage,
                        identity: candidate.identity,
                        hardlink_aliases: aliases,
                    }),
                    Ok(None) => push_issue(
                        &mut issues,
                        &candidate.path,
                        "File changed while it was being hashed".to_owned(),
                    ),
                    Err(error) => push_issue(&mut issues, &candidate.path, error.to_string()),
                }
            }
            for (hash, mut duplicates) in by_hash
                .into_iter()
                .filter(|(_, duplicates)| duplicates.len() > 1)
            {
                duplicates.sort_by(|left, right| left.path.cmp(&right.path));
                let physical_total = duplicates
                    .iter()
                    .map(|file| file.usage.physical)
                    .fold(0_u64, u64::saturating_add);
                groups.push(DuplicateGroup {
                    hash: hash.to_hex().to_string(),
                    logical_size,
                    reclaimable_logical: logical_size
                        .saturating_mul(duplicates.len().saturating_sub(1) as u64),
                    physical_total,
                    files: duplicates,
                });
            }
        }
    }
    groups.sort_by_key(|group| std::cmp::Reverse(group.reclaimable_logical));
    (groups, issues, false)
}

fn sample_hash(path: &Path) -> Result<blake3::Hash> {
    let mut file =
        File::open(path).with_context(|| format!("Could not read {}", path.display()))?;
    let length = file.metadata()?.len();
    let mut hasher = blake3::Hasher::new();
    let mut buffer = vec![0_u8; SAMPLE_SIZE.min(length as usize)];
    file.read_exact(&mut buffer)?;
    hasher.update(&buffer);
    if length > SAMPLE_SIZE as u64 {
        let tail = SAMPLE_SIZE.min(length as usize);
        file.seek(SeekFrom::End(-(tail as i64)))?;
        buffer.resize(tail, 0);
        file.read_exact(&mut buffer)?;
        hasher.update(&buffer);
    }
    Ok(hasher.finalize())
}

fn full_hash(
    record: &FileRecord,
    generation: u64,
    active_generation: &AtomicU64,
) -> Result<Option<blake3::Hash>> {
    let before = fs::symlink_metadata(&record.path)?;
    if !before.file_type().is_file()
        || metadata_identity(&before) != record.identity
        || before.len() != record.usage.logical
    {
        return Ok(None);
    }
    let mut file = File::open(&record.path)?;
    let mut buffer = vec![0_u8; HASH_BUFFER_SIZE];
    let mut hasher = blake3::Hasher::new();
    loop {
        if active_generation.load(Ordering::Relaxed) != generation {
            return Ok(None);
        }
        let read = file.read(&mut buffer)?;
        if read == 0 {
            break;
        }
        hasher.update(&buffer[..read]);
    }
    let after = fs::symlink_metadata(&record.path)?;
    if metadata_identity(&after) != record.identity || after.len() != record.usage.logical {
        return Ok(None);
    }
    Ok(Some(hasher.finalize()))
}

fn normalized_extension(path: &Path) -> Option<String> {
    path.extension()
        .filter(|extension| !extension.is_empty())
        .map(|extension| extension.to_string_lossy().to_lowercase())
}

pub fn category_for_extension(extension: Option<&str>) -> FileCategory {
    match extension.unwrap_or_default() {
        "jpg" | "jpeg" | "png" | "gif" | "webp" | "heic" | "tif" | "tiff" | "svg" | "bmp"
        | "ico" => FileCategory::Image,
        "mp4" | "mov" | "mkv" | "avi" | "webm" | "m4v" | "mpeg" | "mpg" => FileCategory::Video,
        "mp3" | "m4a" | "aac" | "wav" | "flac" | "ogg" | "aiff" => FileCategory::Audio,
        "zip" | "gz" | "tgz" | "bz2" | "xz" | "7z" | "rar" | "tar" | "dmg" | "pkg" => {
            FileCategory::Archive
        }
        "pdf" | "doc" | "docx" | "xls" | "xlsx" | "ppt" | "pptx" | "odt" | "rtf" | "txt" | "md"
        | "pages" | "numbers" | "key" => FileCategory::Document,
        "rs" | "c" | "h" | "cpp" | "hpp" | "m" | "mm" | "swift" | "go" | "py" | "js" | "jsx"
        | "ts" | "tsx" | "java" | "kt" | "rb" | "php" | "sh" | "zsh" | "fish" | "html" | "css"
        | "scss" | "json" | "toml" | "yaml" | "yml" | "xml" | "sql" => FileCategory::Code,
        _ => FileCategory::Other,
    }
}

fn push_issue(issues: &mut Vec<AnalysisIssue>, path: &Path, message: String) {
    if issues.len() < MAX_ISSUES {
        issues.push(AnalysisIssue {
            path: path.to_path_buf(),
            message,
        });
    }
}

#[cfg(unix)]
fn metadata_identity(metadata: &fs::Metadata) -> FileIdentity {
    FileIdentity {
        device: metadata.dev(),
        inode: metadata.ino(),
        modified_seconds: metadata.mtime(),
        modified_nanoseconds: metadata.mtime_nsec(),
    }
}

#[cfg(not(unix))]
fn metadata_identity(metadata: &fs::Metadata) -> FileIdentity {
    let (modified_seconds, modified_nanoseconds) = metadata_modified(metadata);
    FileIdentity {
        device: 0,
        inode: 0,
        modified_seconds,
        modified_nanoseconds,
    }
}

#[cfg(unix)]
fn metadata_modified(metadata: &fs::Metadata) -> (i64, i64) {
    (metadata.mtime(), metadata.mtime_nsec())
}

#[cfg(not(unix))]
fn metadata_modified(metadata: &fs::Metadata) -> (i64, i64) {
    metadata
        .modified()
        .ok()
        .and_then(|value| value.duration_since(UNIX_EPOCH).ok())
        .map_or((0, 0), |value| {
            (value.as_secs() as i64, value.subsec_nanos() as i64)
        })
}

#[cfg(unix)]
fn metadata_device(metadata: &fs::Metadata) -> u64 {
    metadata.dev()
}

#[cfg(not(unix))]
fn metadata_device(_metadata: &fs::Metadata) -> u64 {
    0
}

#[cfg(unix)]
fn metadata_physical_size(metadata: &fs::Metadata) -> u64 {
    metadata.blocks().saturating_mul(512)
}

#[cfg(not(unix))]
fn metadata_physical_size(metadata: &fs::Metadata) -> u64 {
    metadata.len()
}

pub fn now_seconds() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs()
}

#[cfg(test)]
mod tests {
    use std::{fs, sync::atomic::AtomicU64};

    use tempfile::tempdir;

    use super::*;
    use crate::scanner::root_device;

    fn options(path: &Path) -> ScanOptions {
        ScanOptions {
            root_device: root_device(path).unwrap(),
            one_file_system: true,
        }
    }

    #[test]
    fn index_collects_files_directories_extensions_and_ages() {
        let root = tempdir().unwrap();
        fs::create_dir(root.path().join("nested")).unwrap();
        fs::write(root.path().join("photo.JPG"), vec![0_u8; 10]).unwrap();
        fs::write(root.path().join("nested/code.rs"), vec![0_u8; 20]).unwrap();
        #[cfg(unix)]
        std::os::unix::fs::symlink(
            root.path().join("photo.JPG"),
            root.path().join("photo-link"),
        )
        .unwrap();
        let (tx, rx) = unbounded();
        let active = AtomicU64::new(1);

        scan_index(1, root.path(), options(root.path()), &tx, &active);
        drop(tx);
        let mut files = Vec::new();
        let mut directories = Vec::new();
        for event in rx {
            match event {
                AnalysisEvent::FileBatch { files: batch, .. } => files.extend(batch),
                AnalysisEvent::Finished {
                    directories: found, ..
                } => directories = found,
                _ => {}
            }
        }
        assert_eq!(files.len(), 2);
        assert!(
            files
                .iter()
                .any(|file| file.category == FileCategory::Image)
        );
        assert!(
            files
                .iter()
                .any(|file| file.extension.as_deref() == Some("rs"))
        );
        let root_record = directories
            .iter()
            .find(|directory| directory.path == root.path())
            .unwrap();
        assert!(root_record.usage.logical >= 30);
        assert_eq!(root_record.usage.files, 2);
    }

    #[test]
    fn full_index_preserves_sparse_logical_and_physical_sizes() {
        let root = tempdir().unwrap();
        let sparse = File::create(root.path().join("sparse")).unwrap();
        sparse.set_len(8 * 1024 * 1024).unwrap();
        let index = build_index(root.path().to_path_buf(), options(root.path()), false);
        let record = &index.files[0];
        assert_eq!(record.usage.logical, 8 * 1024 * 1024);
        #[cfg(unix)]
        assert!(record.usage.physical < record.usage.logical);
        assert_eq!(index.root_usage().files, 1);
    }

    #[test]
    fn duplicate_detection_hashes_contents_and_collapses_hardlinks() {
        let root = tempdir().unwrap();
        let first = root.path().join("first.bin");
        let second = root.path().join("second.bin");
        let different = root.path().join("different.bin");
        fs::write(&first, b"same-content").unwrap();
        fs::write(&second, b"same-content").unwrap();
        fs::write(&different, b"other-content").unwrap();
        #[cfg(unix)]
        fs::hard_link(&first, root.path().join("first-link.bin")).unwrap();

        let (tx, rx) = unbounded();
        scan_index(
            1,
            root.path(),
            options(root.path()),
            &tx,
            &AtomicU64::new(1),
        );
        drop(tx);
        let files: Vec<_> = rx
            .into_iter()
            .filter_map(|event| match event {
                AnalysisEvent::FileBatch { files, .. } => Some(files),
                _ => None,
            })
            .flatten()
            .collect();
        let (groups, issues, cancelled) = detect_duplicates(&files, 1, &AtomicU64::new(1));
        assert!(!cancelled);
        assert!(issues.is_empty());
        assert_eq!(groups.len(), 1);
        assert_eq!(groups[0].files.len(), 2);
        #[cfg(unix)]
        assert_eq!(groups[0].files[0].hardlink_aliases.len(), 1);
    }

    #[test]
    fn cancellation_omits_finished_event() {
        let root = tempdir().unwrap();
        fs::write(root.path().join("file"), b"data").unwrap();
        let (tx, rx) = unbounded();
        scan_index(
            1,
            root.path(),
            options(root.path()),
            &tx,
            &AtomicU64::new(2),
        );
        drop(tx);
        assert!(
            rx.into_iter()
                .all(|event| !matches!(event, AnalysisEvent::Finished { .. }))
        );
    }
}
