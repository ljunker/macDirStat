use std::{
    collections::HashSet,
    path::{Path, PathBuf},
    sync::mpsc::{Receiver, Sender, TryRecvError, channel},
    time::{Duration, Instant},
};

use anyhow::{Context, Result};
use notify::{Event, EventKind, RecommendedWatcher, RecursiveMode, Watcher};

const EVENT_STORM_LIMIT: usize = 1_000;

#[derive(Debug, Default, Eq, PartialEq)]
pub struct WatchBatch {
    pub paths: Vec<PathBuf>,
    pub full_rescan: bool,
    pub warning: Option<String>,
}

pub struct WatchService {
    _watcher: RecommendedWatcher,
    receiver: Receiver<notify::Result<Event>>,
    root: PathBuf,
    debounce: Duration,
    accumulator: EventAccumulator,
}

impl WatchService {
    pub fn start(root: PathBuf, debounce: Duration) -> Result<Self> {
        let (sender, receiver) = channel();
        let mut watcher = notify::recommended_watcher(move |event| send_event(&sender, event))
            .context("Could not initialize filesystem watcher")?;
        watcher
            .watch(&root, RecursiveMode::Recursive)
            .with_context(|| format!("Could not watch {}", root.display()))?;
        Ok(Self {
            _watcher: watcher,
            receiver,
            root,
            debounce,
            accumulator: EventAccumulator::default(),
        })
    }

    pub fn poll(&mut self) -> Option<WatchBatch> {
        loop {
            match self.receiver.try_recv() {
                Ok(Ok(event)) => self.accumulator.push_event(&self.root, event),
                Ok(Err(error)) => self.accumulator.push_error(error.to_string()),
                Err(TryRecvError::Empty) => break,
                Err(TryRecvError::Disconnected) => {
                    self.accumulator
                        .push_error("Filesystem watcher stopped unexpectedly".to_owned());
                    break;
                }
            }
        }
        self.accumulator.take_if_ready(self.debounce)
    }
}

fn send_event(sender: &Sender<notify::Result<Event>>, event: notify::Result<Event>) {
    let _ = sender.send(event);
}

#[derive(Default)]
struct EventAccumulator {
    paths: HashSet<PathBuf>,
    first_event: Option<Instant>,
    full_rescan: bool,
    warning: Option<String>,
}

impl EventAccumulator {
    fn push_event(&mut self, root: &Path, event: Event) {
        if self.first_event.is_none() {
            self.first_event = Some(Instant::now());
        }
        if matches!(event.kind, EventKind::Other) {
            self.full_rescan = true;
        }
        for path in event.paths {
            let path = if path.is_absolute() {
                path
            } else {
                root.join(path)
            };
            if path.starts_with(root) {
                self.paths.insert(path);
            }
        }
        if self.paths.len() > EVENT_STORM_LIMIT {
            self.paths.clear();
            self.full_rescan = true;
            self.warning = Some("Filesystem event storm; refreshing the root once".to_owned());
        }
    }

    fn push_error(&mut self, message: String) {
        if self.first_event.is_none() {
            self.first_event = Some(Instant::now());
        }
        self.full_rescan = true;
        self.warning = Some(format!("Watcher warning: {message}"));
    }

    fn take_if_ready(&mut self, debounce: Duration) -> Option<WatchBatch> {
        let ready = self
            .first_event
            .is_some_and(|started| started.elapsed() >= debounce);
        if !ready {
            return None;
        }
        self.first_event = None;
        let mut paths: Vec<_> = self.paths.drain().collect();
        paths.sort();
        Some(WatchBatch {
            paths,
            full_rescan: std::mem::take(&mut self.full_rescan),
            warning: self.warning.take(),
        })
    }
}

#[cfg(test)]
mod tests {
    use notify::event::{CreateKind, ModifyKind};

    use super::*;

    fn event(kind: EventKind, paths: Vec<PathBuf>) -> Event {
        Event {
            kind,
            paths,
            attrs: Default::default(),
        }
    }

    #[test]
    fn coalesces_duplicate_paths_and_resolves_relative_paths() {
        let root = Path::new("/tmp/root");
        let mut accumulator = EventAccumulator::default();
        accumulator.push_event(
            root,
            event(
                EventKind::Modify(ModifyKind::Any),
                vec![PathBuf::from("child"), root.join("child")],
            ),
        );
        let batch = accumulator.take_if_ready(Duration::ZERO).unwrap();
        assert_eq!(batch.paths, [root.join("child")]);
        assert!(!batch.full_rescan);
    }

    #[test]
    fn ignores_paths_outside_root_and_escalates_watcher_errors() {
        let root = Path::new("/tmp/root");
        let mut accumulator = EventAccumulator::default();
        accumulator.push_event(
            root,
            event(
                EventKind::Create(CreateKind::Any),
                vec![PathBuf::from("/outside")],
            ),
        );
        accumulator.push_error("overflow".to_owned());
        let batch = accumulator.take_if_ready(Duration::ZERO).unwrap();
        assert!(batch.paths.is_empty());
        assert!(batch.full_rescan);
        assert!(batch.warning.unwrap().contains("overflow"));
    }

    #[test]
    fn event_storm_collapses_to_one_full_refresh() {
        let root = Path::new("/tmp/root");
        let mut accumulator = EventAccumulator::default();
        for index in 0..=EVENT_STORM_LIMIT {
            accumulator.push_event(
                root,
                event(
                    EventKind::Modify(ModifyKind::Any),
                    vec![root.join(format!("file-{index}"))],
                ),
            );
        }
        let batch = accumulator.take_if_ready(Duration::ZERO).unwrap();
        assert!(batch.full_rescan);
        assert!(batch.paths.is_empty());
    }
}
