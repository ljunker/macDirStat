use std::{io::Write, path::PathBuf};

use anyhow::{Context, Result};

use crate::{
    analytics::build_index,
    config::Settings,
    scanner::{ScanOptions, root_device},
    snapshot::{SnapshotStore, compare_indices, load_compatible_snapshot, write_export},
};

pub fn run(root: PathBuf, settings: &Settings) -> Result<()> {
    let destination = settings
        .export_json
        .as_deref()
        .context("Missing --export-json destination")?;
    let options = ScanOptions {
        root_device: root_device(&root)?,
        one_file_system: settings.one_file_system,
    };
    let index = build_index(root, options, settings.detect_duplicates);

    let store = SnapshotStore::new(settings.paths.snapshots_dir.clone(), settings.snapshots);
    let rolling = store.process_completed(&index)?;
    if let Some(warning) = rolling.warning {
        eprintln!("macDirStat: {warning}");
    }
    let comparison = if let Some(path) = &settings.compare_snapshot {
        let previous = load_compatible_snapshot(path, &index)?;
        Some(compare_indices(&previous.index, &index))
    } else {
        rolling.comparison
    };
    write_export(destination, &index, comparison)?;

    let usage = index.root_usage();
    let duplicate_groups = index.duplicates.as_ref().map_or(0, Vec::len);
    let mut stderr = std::io::stderr().lock();
    writeln!(
        stderr,
        "macDirStat: indexed {} files, {} directories, {} duplicate groups, {} issue(s)",
        usage.files,
        index.directories.len(),
        duplicate_groups,
        index.issues.len(),
    )?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use std::fs;

    use tempfile::tempdir;

    use super::*;
    use crate::snapshot::load_snapshot;

    #[test]
    fn writes_complete_headless_export_with_duplicates() {
        let directory = tempdir().unwrap();
        let root = directory.path().join("root");
        fs::create_dir(&root).unwrap();
        fs::write(root.join("a"), b"same").unwrap();
        fs::write(root.join("b"), b"same").unwrap();
        let root = fs::canonicalize(root).unwrap();
        let export = directory.path().join("export.json");
        let mut settings = Settings::for_tests(1, directory.path());
        settings.export_json = Some(export.clone());
        settings.detect_duplicates = true;

        run(root, &settings).unwrap();

        let snapshot = load_snapshot(&export).unwrap();
        assert!(snapshot.index.complete);
        assert_eq!(snapshot.index.files.len(), 2);
        assert_eq!(snapshot.index.duplicates.unwrap().len(), 1);
    }
}
