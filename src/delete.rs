use std::{
    fs,
    path::{Path, PathBuf},
    sync::Arc,
    thread::{self, JoinHandle},
};

use anyhow::{Context, Result};
use crossbeam_channel::{Receiver, Sender, TryRecvError, unbounded};
use thiserror::Error;

use crate::tree::NodeId;

pub trait Trash: Send + Sync + 'static {
    fn delete(&self, path: &Path) -> Result<(), String>;
}

struct SystemTrash;

impl Trash for SystemTrash {
    fn delete(&self, path: &Path) -> Result<(), String> {
        trash::delete(path).map_err(|error| error.to_string())
    }
}

#[derive(Clone, Debug)]
pub struct DeleteItem {
    pub node_id: Option<NodeId>,
    pub path: PathBuf,
}

#[derive(Debug)]
pub struct DeleteRequest {
    pub generation: u64,
    pub root: PathBuf,
    pub items: Vec<DeleteItem>,
}

#[derive(Debug)]
pub struct DeleteFailure {
    pub path: PathBuf,
    pub message: String,
}

#[derive(Debug)]
pub struct DeleteResult {
    pub generation: u64,
    pub moved: Vec<DeleteItem>,
    pub failures: Vec<DeleteFailure>,
}

pub struct FileOperationWorker {
    request_tx: Option<Sender<DeleteRequest>>,
    result_rx: Receiver<DeleteResult>,
    handle: Option<JoinHandle<()>>,
}

impl FileOperationWorker {
    pub fn new() -> Result<Self> {
        Self::with_trash(Arc::new(SystemTrash))
    }

    fn with_trash(trash: Arc<dyn Trash>) -> Result<Self> {
        let (request_tx, request_rx) = unbounded::<DeleteRequest>();
        let (result_tx, result_rx) = unbounded();
        let handle = thread::Builder::new()
            .name("macDirStat-trash".to_owned())
            .spawn(move || {
                for request in request_rx {
                    let mut verified = Vec::with_capacity(request.items.len());
                    let mut failures = Vec::new();
                    for item in request.items {
                        match validate_delete_target(&request.root, &item.path) {
                            Ok(path) => verified.push((item, path)),
                            Err(error) => failures.push(DeleteFailure {
                                path: item.path,
                                message: error.to_string(),
                            }),
                        }
                    }

                    let mut moved = Vec::new();
                    if failures.is_empty() {
                        for (item, verified_path) in verified {
                            match trash.delete(&verified_path) {
                                Ok(()) => moved.push(item),
                                Err(message) => failures.push(DeleteFailure {
                                    path: item.path,
                                    message,
                                }),
                            }
                        }
                    } else {
                        for (item, _) in verified {
                            failures.push(DeleteFailure {
                                path: item.path,
                                message: "Batch cancelled because another item failed validation"
                                    .to_owned(),
                            });
                        }
                    }
                    if result_tx
                        .send(DeleteResult {
                            generation: request.generation,
                            moved,
                            failures,
                        })
                        .is_err()
                    {
                        break;
                    }
                }
            })
            .context("Could not start Trash worker")?;

        Ok(Self {
            request_tx: Some(request_tx),
            result_rx,
            handle: Some(handle),
        })
    }

    pub fn send(&self, request: DeleteRequest) -> Result<()> {
        self.request_tx
            .as_ref()
            .context("Trash worker is shut down")?
            .send(request)
            .context("Trash worker stopped unexpectedly")
    }

    pub fn try_recv(&self) -> Result<DeleteResult, TryRecvError> {
        self.result_rx.try_recv()
    }
}

impl Drop for FileOperationWorker {
    fn drop(&mut self) {
        self.request_tx.take();
        if let Some(handle) = self.handle.take() {
            let _ = handle.join();
        }
    }
}

#[derive(Debug, Error)]
pub enum DeleteValidationError {
    #[error("The scan root cannot be moved to Trash")]
    Root,
    #[error("The selected path has no valid file name")]
    MissingName,
    #[error("The selected path is outside the current scan root")]
    OutsideRoot,
    #[error("Cannot verify the parent directory: {0}")]
    Parent(String),
    #[error("The selected item no longer exists: {0}")]
    Missing(String),
}

pub fn validate_delete_target(
    root: &Path,
    target: &Path,
) -> Result<PathBuf, DeleteValidationError> {
    if target == root {
        return Err(DeleteValidationError::Root);
    }
    let name = target
        .file_name()
        .ok_or(DeleteValidationError::MissingName)?;
    if name == "." || name == ".." {
        return Err(DeleteValidationError::MissingName);
    }
    let parent = target.parent().ok_or(DeleteValidationError::MissingName)?;
    let canonical_parent = fs::canonicalize(parent)
        .map_err(|error| DeleteValidationError::Parent(error.to_string()))?;
    if !canonical_parent.starts_with(root) {
        return Err(DeleteValidationError::OutsideRoot);
    }

    let verified_target = canonical_parent.join(name);
    fs::symlink_metadata(&verified_target)
        .map_err(|error| DeleteValidationError::Missing(error.to_string()))?;
    Ok(verified_target)
}

#[cfg(test)]
mod tests {
    use std::{
        sync::{Arc, Mutex},
        time::Duration,
    };

    use tempfile::tempdir;

    use super::*;
    use crate::tree::Tree;

    #[derive(Default)]
    struct MockTrash {
        paths: Mutex<Vec<PathBuf>>,
    }

    impl Trash for MockTrash {
        fn delete(&self, path: &Path) -> Result<(), String> {
            self.paths.lock().unwrap().push(path.to_path_buf());
            Ok(())
        }
    }

    #[test]
    fn only_requested_paths_are_sent_to_trash() {
        let mock = Arc::new(MockTrash::default());
        let worker = FileOperationWorker::with_trash(mock.clone()).unwrap();
        let root_dir = tempdir().unwrap();
        let root = fs::canonicalize(root_dir.path()).unwrap();
        let tree = Tree::new(root.clone());
        let first = root.join("first");
        let second = root.join("second");
        fs::write(&first, b"data").unwrap();
        fs::write(&second, b"data").unwrap();
        worker
            .send(DeleteRequest {
                generation: 1,
                root,
                items: vec![
                    DeleteItem {
                        node_id: Some(tree.root_id),
                        path: first.clone(),
                    },
                    DeleteItem {
                        node_id: Some(tree.root_id),
                        path: second.clone(),
                    },
                ],
            })
            .unwrap();

        let result = worker
            .result_rx
            .recv_timeout(Duration::from_secs(1))
            .unwrap();
        assert!(result.failures.is_empty());
        assert_eq!(result.moved.len(), 2);
        assert_eq!(*mock.paths.lock().unwrap(), [first, second]);
    }

    #[test]
    fn batch_validation_happens_before_any_item_is_trashed() {
        let mock = Arc::new(MockTrash::default());
        let worker = FileOperationWorker::with_trash(mock.clone()).unwrap();
        let root_dir = tempdir().unwrap();
        let root = fs::canonicalize(root_dir.path()).unwrap();
        let tree = Tree::new(root.clone());
        let valid = root.join("valid");
        fs::write(&valid, b"data").unwrap();
        worker
            .send(DeleteRequest {
                generation: 1,
                root: root.clone(),
                items: vec![
                    DeleteItem {
                        node_id: Some(tree.root_id),
                        path: valid,
                    },
                    DeleteItem {
                        node_id: Some(tree.root_id),
                        path: root.join("missing"),
                    },
                ],
            })
            .unwrap();

        let result = worker
            .result_rx
            .recv_timeout(Duration::from_secs(1))
            .unwrap();
        assert!(result.moved.is_empty());
        assert_eq!(result.failures.len(), 2);
        assert!(mock.paths.lock().unwrap().is_empty());
    }

    #[test]
    fn rejects_root_and_outside_paths() {
        let root_dir = tempdir().unwrap();
        let outside_dir = tempdir().unwrap();
        let root = fs::canonicalize(root_dir.path()).unwrap();
        let outside_file = outside_dir.path().join("outside");
        fs::write(&outside_file, b"data").unwrap();

        assert!(matches!(
            validate_delete_target(&root, &root),
            Err(DeleteValidationError::Root)
        ));
        assert!(matches!(
            validate_delete_target(&root, &outside_file),
            Err(DeleteValidationError::OutsideRoot)
        ));
    }

    #[cfg(unix)]
    #[test]
    fn accepts_symlink_itself_without_following_target() {
        use std::os::unix::fs::symlink;

        let root_dir = tempdir().unwrap();
        let outside_dir = tempdir().unwrap();
        let root = fs::canonicalize(root_dir.path()).unwrap();
        let link = root.join("outside-link");
        symlink(outside_dir.path(), &link).unwrap();

        assert_eq!(validate_delete_target(&root, &link).unwrap(), link);
    }
}
