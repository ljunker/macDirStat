use std::{
    collections::HashMap,
    ffi::OsString,
    fs::{self, File, OpenOptions},
    io::{BufReader, BufWriter, Write},
    path::{Path, PathBuf},
    time::{Duration, SystemTime, UNIX_EPOCH},
};

use serde::{Deserialize, Serialize};

use crate::tree::{FileIdentity, UsageStats};

const CACHE_SCHEMA: u32 = 1;
const MAX_CACHE_ENTRIES: usize = 50_000;

#[derive(Clone, Debug, Serialize, Deserialize)]
struct CacheRecord {
    path: OsString,
    identity: FileIdentity,
    usage: UsageStats,
    one_file_system: bool,
    saved_at: u64,
}

#[derive(Debug, Serialize, Deserialize)]
struct CacheFile {
    schema: u32,
    records: Vec<CacheRecord>,
}

#[derive(Debug)]
pub struct ScanCache {
    path: PathBuf,
    ttl: Duration,
    enabled: bool,
    dirty: bool,
    records: HashMap<OsString, CacheRecord>,
}

impl ScanCache {
    pub fn load(path: PathBuf, enabled: bool, ttl: Duration) -> (Self, Option<String>) {
        let mut cache = Self {
            path,
            ttl,
            enabled,
            dirty: false,
            records: HashMap::new(),
        };
        if !enabled || !cache.path.exists() {
            return (cache, None);
        }

        let result = File::open(&cache.path)
            .map(BufReader::new)
            .map_err(|error| error.to_string())
            .and_then(|reader| {
                serde_json::from_reader::<_, CacheFile>(reader).map_err(|error| error.to_string())
            });
        match result {
            Ok(mut stored) if stored.schema == CACHE_SCHEMA => {
                let now = now_seconds();
                stored
                    .records
                    .retain(|record| now.saturating_sub(record.saved_at) <= cache.ttl.as_secs());
                stored
                    .records
                    .sort_by_key(|record| std::cmp::Reverse(record.saved_at));
                for record in stored.records.into_iter().take(MAX_CACHE_ENTRIES) {
                    cache.records.insert(record.path.clone(), record);
                }
                (cache, None)
            }
            Ok(_) => (
                cache,
                Some("Ignoring cache with an unsupported schema".to_owned()),
            ),
            Err(error) => (
                cache,
                Some(format!("Ignoring unreadable scan cache: {error}")),
            ),
        }
    }

    pub fn lookup(
        &self,
        path: &Path,
        identity: FileIdentity,
        one_file_system: bool,
    ) -> Option<UsageStats> {
        if !self.enabled {
            return None;
        }
        let record = self.records.get(path.as_os_str())?;
        (record.identity == identity
            && record.one_file_system == one_file_system
            && now_seconds().saturating_sub(record.saved_at) <= self.ttl.as_secs())
        .then_some(record.usage)
    }

    pub fn insert(
        &mut self,
        path: &Path,
        identity: FileIdentity,
        usage: UsageStats,
        one_file_system: bool,
    ) {
        if !self.enabled {
            return;
        }
        let record = CacheRecord {
            path: path.as_os_str().to_os_string(),
            identity,
            usage,
            one_file_system,
            saved_at: now_seconds(),
        };
        self.records.insert(record.path.clone(), record);
        self.prune();
        self.dirty = true;
    }

    pub fn invalidate_subtree(&mut self, root: &Path) {
        if !self.enabled {
            return;
        }
        let before = self.records.len();
        self.records
            .retain(|path, _| !Path::new(path).starts_with(root));
        self.dirty |= self.records.len() != before;
    }

    pub fn invalidate_path(&mut self, path: &Path) {
        if self.enabled && self.records.remove(path.as_os_str()).is_some() {
            self.dirty = true;
        }
    }

    pub fn flush(&mut self) -> Result<(), String> {
        if !self.enabled || !self.dirty {
            return Ok(());
        }
        let Some(parent) = self.path.parent() else {
            return Err(format!("Invalid cache path: {}", self.path.display()));
        };
        fs::create_dir_all(parent).map_err(|error| {
            format!(
                "Could not create cache directory {}: {error}",
                parent.display()
            )
        })?;

        let mut records: Vec<_> = self.records.values().cloned().collect();
        records.sort_by_key(|record| std::cmp::Reverse(record.saved_at));
        records.truncate(MAX_CACHE_ENTRIES);
        let stored = CacheFile {
            schema: CACHE_SCHEMA,
            records,
        };
        let temp_path = self
            .path
            .with_extension(format!("json.tmp-{}", std::process::id()));
        let write_result = (|| -> Result<(), String> {
            let file = OpenOptions::new()
                .create(true)
                .truncate(true)
                .write(true)
                .open(&temp_path)
                .map_err(|error| format!("Could not create {}: {error}", temp_path.display()))?;
            let mut writer = BufWriter::new(file);
            serde_json::to_writer(&mut writer, &stored)
                .map_err(|error| format!("Could not encode scan cache: {error}"))?;
            writer
                .flush()
                .map_err(|error| format!("Could not flush scan cache: {error}"))?;
            fs::rename(&temp_path, &self.path).map_err(|error| {
                format!("Could not replace cache {}: {error}", self.path.display())
            })?;
            Ok(())
        })();
        if write_result.is_err() {
            let _ = fs::remove_file(&temp_path);
        } else {
            self.dirty = false;
        }
        write_result
    }

    fn prune(&mut self) {
        if self.records.len() <= MAX_CACHE_ENTRIES {
            return;
        }
        let mut paths: Vec<_> = self
            .records
            .values()
            .map(|record| (record.saved_at, record.path.clone()))
            .collect();
        paths.sort_by_key(|(saved_at, _)| std::cmp::Reverse(*saved_at));
        for (_, path) in paths.into_iter().skip(MAX_CACHE_ENTRIES) {
            self.records.remove(&path);
        }
    }
}

fn now_seconds() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs()
}

#[cfg(test)]
mod tests {
    use std::ffi::OsString;

    #[cfg(unix)]
    use std::os::unix::ffi::OsStringExt;
    use tempfile::tempdir;

    use super::*;

    fn identity(modified_seconds: i64) -> FileIdentity {
        FileIdentity {
            device: 1,
            inode: 2,
            modified_seconds,
            modified_nanoseconds: 3,
        }
    }

    #[test]
    fn persists_hits_and_rejects_changed_identity() {
        let directory = tempdir().unwrap();
        let cache_path = directory.path().join("cache.json");
        let path = Path::new("/tmp/example");
        let usage = UsageStats {
            logical: 10,
            physical: 16,
            files: 2,
        };
        let (mut cache, warning) =
            ScanCache::load(cache_path.clone(), true, Duration::from_secs(60));
        assert!(warning.is_none());
        cache.insert(path, identity(4), usage, true);
        cache.flush().unwrap();

        let (cache, warning) = ScanCache::load(cache_path, true, Duration::from_secs(60));
        assert!(warning.is_none());
        assert_eq!(cache.lookup(path, identity(4), true), Some(usage));
        assert_eq!(cache.lookup(path, identity(5), true), None);
    }

    #[test]
    fn corrupt_cache_is_non_fatal() {
        let directory = tempdir().unwrap();
        let cache_path = directory.path().join("cache.json");
        fs::write(&cache_path, "not json").unwrap();
        let (_, warning) = ScanCache::load(cache_path, true, Duration::from_secs(60));
        assert!(warning.unwrap().contains("Ignoring unreadable"));
    }

    #[test]
    fn unsupported_schema_is_atomically_replaced() {
        let directory = tempdir().unwrap();
        let cache_path = directory.path().join("cache.json");
        fs::write(&cache_path, r#"{"schema":999,"records":[]}"#).unwrap();
        let (mut cache, warning) =
            ScanCache::load(cache_path.clone(), true, Duration::from_secs(60));
        assert!(warning.unwrap().contains("unsupported schema"));

        cache.insert(
            Path::new("/replacement"),
            identity(4),
            UsageStats::default(),
            true,
        );
        cache.flush().unwrap();

        let stored: CacheFile = serde_json::from_reader(File::open(&cache_path).unwrap()).unwrap();
        assert_eq!(stored.schema, CACHE_SCHEMA);
        let temp_path = cache_path.with_extension(format!("json.tmp-{}", std::process::id()));
        assert!(!temp_path.exists());
    }

    #[cfg(unix)]
    #[test]
    fn preserves_non_utf8_paths() {
        let directory = tempdir().unwrap();
        let cache_path = directory.path().join("cache.json");
        let path = PathBuf::from(OsString::from_vec(vec![b'/', b't', b'm', b'p', b'/', 0xff]));
        let usage = UsageStats {
            logical: 1,
            physical: 8,
            files: 1,
        };
        let (mut cache, _) = ScanCache::load(cache_path.clone(), true, Duration::from_secs(60));
        cache.insert(&path, identity(4), usage, true);
        cache.flush().unwrap();
        let (cache, warning) = ScanCache::load(cache_path, true, Duration::from_secs(60));
        assert!(warning.is_none());
        assert_eq!(cache.lookup(&path, identity(4), true), Some(usage));
    }

    #[test]
    fn subtree_invalidation_removes_only_descendants() {
        let directory = tempdir().unwrap();
        let (mut cache, _) = ScanCache::load(
            directory.path().join("cache.json"),
            true,
            Duration::from_secs(60),
        );
        let usage = UsageStats::default();
        cache.insert(Path::new("/a/child"), identity(1), usage, true);
        cache.insert(Path::new("/b"), identity(1), usage, true);
        cache.invalidate_subtree(Path::new("/a"));
        assert!(
            cache
                .lookup(Path::new("/a/child"), identity(1), true)
                .is_none()
        );
        assert!(cache.lookup(Path::new("/b"), identity(1), true).is_some());
    }

    #[test]
    fn expired_entries_are_not_returned() {
        let directory = tempdir().unwrap();
        let (mut cache, _) = ScanCache::load(
            directory.path().join("cache.json"),
            true,
            Duration::from_secs(1),
        );
        let path = Path::new("/expired");
        cache.insert(path, identity(1), UsageStats::default(), true);
        cache.records.get_mut(path.as_os_str()).unwrap().saved_at = now_seconds().saturating_sub(2);
        assert!(cache.lookup(path, identity(1), true).is_none());
    }
}
