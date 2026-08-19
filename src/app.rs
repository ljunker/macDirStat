use std::{
    collections::{HashMap, HashSet},
    fs, io,
    path::{Path, PathBuf},
    time::{Duration, Instant},
};

use anyhow::{Result, anyhow};
use crossterm::{
    event::{
        self, DisableMouseCapture, EnableMouseCapture, Event, KeyCode, KeyEvent, KeyEventKind,
        KeyModifiers, MouseButton, MouseEvent, MouseEventKind,
    },
    execute,
};
use ratatui::{DefaultTerminal, layout::Rect};

use crate::{
    analytics::{AnalysisEvent, AnalysisIndex, AnalysisWorker, FileRecord, GroupStats},
    cache::ScanCache,
    config::Settings,
    delete::{DeleteItem, DeleteRequest, FileOperationWorker, validate_delete_target},
    filter::FilterExpression,
    format::format_size,
    scanner::{ScanOptions, root_device},
    snapshot::{
        SnapshotComparison, SnapshotStore, compare_indices, load_compatible_snapshot, write_export,
    },
    theme::ThemeKind,
    tree::{
        Node, NodeId, NodeKind, ScanState, SizeMode, SortDirection, SortKey, SortSpec, Tree,
        VisibleNode,
    },
    ui,
    watcher::WatchService,
    worker::{ScanJob, WorkerEvent, WorkerPool},
};

const EVENT_POLL: Duration = Duration::from_millis(33);
const SORT_INTERVAL: Duration = Duration::from_millis(200);
const CACHE_FLUSH_INTERVAL: Duration = Duration::from_secs(2);
const DOUBLE_CLICK_INTERVAL: Duration = Duration::from_millis(400);
const MAX_SCAN_EVENTS_PER_TICK: usize = 128;
const MAX_ANALYSIS_EVENTS_PER_TICK: usize = 16;
const MAX_FILE_EVENTS_PER_TICK: usize = 16;
const MAX_ANALYSIS_ROWS: usize = 100_000;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum PromptKind {
    Search,
    Filter,
    Export,
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub enum ViewKind {
    #[default]
    Tree,
    Largest,
    Types,
    Duplicates,
    Changes,
}

impl ViewKind {
    pub const ALL: [Self; 5] = [
        Self::Tree,
        Self::Largest,
        Self::Types,
        Self::Duplicates,
        Self::Changes,
    ];

    pub fn label(self) -> &'static str {
        match self {
            Self::Tree => "Tree",
            Self::Largest => "Largest",
            Self::Types => "Types",
            Self::Duplicates => "Duplicates",
            Self::Changes => "Changes",
        }
    }

    fn next(self, backwards: bool) -> Self {
        let index = Self::ALL.iter().position(|view| *view == self).unwrap_or(0);
        let next = if backwards {
            index.checked_sub(1).unwrap_or(Self::ALL.len() - 1)
        } else {
            (index + 1) % Self::ALL.len()
        };
        Self::ALL[next]
    }
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub enum AnalysisStatus {
    #[default]
    Idle,
    Indexing,
    Ready,
    Hashing,
}

#[derive(Clone, Debug)]
pub struct AnalysisRow {
    pub label: String,
    pub path: Option<PathBuf>,
    pub usage: crate::tree::UsageStats,
    pub files: u64,
    pub detail: String,
    pub indent: usize,
}

#[derive(Debug)]
pub enum AppMode {
    Browse,
    Help,
    Input {
        kind: PromptKind,
        value: String,
        previous_filter: Option<String>,
    },
    ConfirmDelete {
        items: Vec<DeleteItem>,
        multi: bool,
    },
    Deleting {
        items: Vec<DeleteItem>,
        multi: bool,
    },
    ErrorDialog(String),
}

pub struct App {
    pub root: PathBuf,
    pub tree: Tree,
    pub visible: Vec<VisibleNode>,
    pub selected: Option<NodeId>,
    pub scroll_offset: usize,
    pub page_size: usize,
    pub mode: AppMode,
    pub workers: usize,
    pub pending_jobs: usize,
    pub errors: usize,
    pub notice: Option<String>,
    pub size_mode: SizeMode,
    pub sort: SortSpec,
    pub theme: ThemeKind,
    pub detail_panel: bool,
    pub mouse_enabled: bool,
    pub filter_query: Option<String>,
    pub search_query: Option<String>,
    pub help_scroll: usize,
    pub list_area: Rect,
    pub cache_hits: usize,
    pub mounts_skipped: usize,
    pub active_view: ViewKind,
    pub analysis_status: AnalysisStatus,
    pub analysis_index: Option<AnalysisIndex>,
    pub analysis_rows: Vec<AnalysisRow>,
    pub analysis_selected: usize,
    pub analysis_scroll: usize,
    pub analysis_rows_truncated: bool,
    analysis_found_usage: crate::tree::UsageStats,
    pub comparison: Option<SnapshotComparison>,
    pub watch_enabled: bool,
    marked: HashSet<PathBuf>,
    filter_expression: Option<FilterExpression>,
    analysis_refresh_pending: HashSet<PathBuf>,
    analysis_previous_usage: HashMap<PathBuf, crate::tree::UsageStats>,
    generation: u64,
    root_device: u64,
    one_file_system: bool,
    scan_pool: WorkerPool,
    analysis_worker: AnalysisWorker,
    file_worker: FileOperationWorker,
    cache: ScanCache,
    snapshot_store: SnapshotStore,
    compare_snapshot: Option<PathBuf>,
    detect_duplicates: bool,
    watch_debounce: Duration,
    watcher: Option<WatchService>,
    sort_dirty: bool,
    last_sort: Instant,
    last_cache_flush: Instant,
    should_quit: bool,
    quit_after_delete: bool,
    pending_select_path: Option<PathBuf>,
    last_click: Option<(NodeId, Instant)>,
}

impl App {
    #[cfg(test)]
    pub fn new(root: PathBuf, workers: usize) -> Result<Self> {
        let settings = Settings::for_tests(workers, &root);
        Self::with_settings(root, settings)
    }

    pub fn with_settings(root: PathBuf, settings: Settings) -> Result<Self> {
        let generation = 1;
        let tree = Tree::new(root.clone());
        let root_id = tree.root_id;
        let root_device = root_device(&root)?;
        let (cache, cache_warning) = ScanCache::load(
            settings.paths.cache_file.clone(),
            settings.cache_enabled,
            settings.cache_ttl,
        );
        let snapshot_store =
            SnapshotStore::new(settings.paths.snapshots_dir.clone(), settings.snapshots);
        let watcher = if settings.watch {
            Some(WatchService::start(root.clone(), settings.watch_debounce)?)
        } else {
            None
        };
        let mut app = Self {
            root,
            tree,
            visible: Vec::new(),
            selected: None,
            scroll_offset: 0,
            page_size: 1,
            mode: AppMode::Browse,
            workers: settings.workers,
            pending_jobs: 0,
            errors: 0,
            notice: cache_warning,
            size_mode: settings.size_mode,
            sort: settings.sort,
            theme: settings.theme,
            detail_panel: settings.detail_panel,
            mouse_enabled: settings.mouse,
            filter_query: None,
            search_query: None,
            help_scroll: 0,
            list_area: Rect::default(),
            cache_hits: 0,
            mounts_skipped: 0,
            active_view: ViewKind::Tree,
            analysis_status: AnalysisStatus::Idle,
            analysis_index: None,
            analysis_rows: Vec::new(),
            analysis_selected: 0,
            analysis_scroll: 0,
            analysis_rows_truncated: false,
            analysis_found_usage: crate::tree::UsageStats::default(),
            comparison: None,
            watch_enabled: settings.watch,
            marked: HashSet::new(),
            filter_expression: None,
            analysis_refresh_pending: HashSet::new(),
            analysis_previous_usage: HashMap::new(),
            generation,
            root_device,
            one_file_system: settings.one_file_system,
            scan_pool: WorkerPool::new(settings.workers, generation)?,
            analysis_worker: AnalysisWorker::new(generation)?,
            file_worker: FileOperationWorker::new()?,
            cache,
            snapshot_store,
            compare_snapshot: settings.compare_snapshot,
            detect_duplicates: settings.detect_duplicates,
            watch_debounce: settings.watch_debounce,
            watcher,
            sort_dirty: false,
            last_sort: Instant::now(),
            last_cache_flush: Instant::now(),
            should_quit: false,
            quit_after_delete: false,
            pending_select_path: None,
            last_click: None,
        };
        app.queue_children(root_id)?;
        if app.detect_duplicates || app.compare_snapshot.is_some() {
            app.ensure_analysis();
        }
        Ok(app)
    }

    pub fn run(&mut self, terminal: &mut DefaultTerminal) -> io::Result<()> {
        let mut mouse_capture = MouseCapture::new(self.mouse_enabled)?;
        while !self.should_quit {
            self.drain_events();
            self.maybe_sort();
            self.maybe_flush_cache();
            terminal.draw(|frame| ui::render(frame, self))?;

            if event::poll(EVENT_POLL)? {
                match event::read()? {
                    Event::Key(key)
                        if matches!(key.kind, KeyEventKind::Press | KeyEventKind::Repeat) =>
                    {
                        self.handle_key(key);
                    }
                    Event::Mouse(mouse) if self.mouse_enabled => self.handle_mouse(mouse),
                    Event::Resize(_, _) => {}
                    _ => {}
                }
                mouse_capture.set_enabled(self.mouse_enabled)?;
            }
        }
        if let Err(error) = self.cache.flush() {
            self.notice = Some(error);
        }
        Ok(())
    }

    pub fn status_text(&self) -> String {
        let usage = self.tree.root_known_usage();
        let size = format_size(usage.size(self.size_mode));
        let progress = if self.active_view != ViewKind::Tree {
            let files = self
                .analysis_index
                .as_ref()
                .map_or(0, |index| index.files.len());
            match self.analysis_status {
                AnalysisStatus::Idle => "Analysis idle".to_owned(),
                AnalysisStatus::Indexing => format!("Analyzing: {files} files indexed"),
                AnalysisStatus::Hashing => format!("Hashing duplicates across {files} files"),
                AnalysisStatus::Ready => format!("Analysis complete: {files} files"),
            }
        } else if self.pending_jobs == 0 {
            format!(
                "Scan complete | {size} | {} files | {} errors",
                usage.files, self.errors
            )
        } else {
            format!(
                "Scanning: {} jobs | {size} found | {} workers | {} errors",
                self.pending_jobs, self.workers, self.errors
            )
        };
        let mut parts = vec![progress];
        if !self.marked.is_empty() {
            parts.push(format!("{} marked", self.marked.len()));
        }
        if self.cache_hits > 0 {
            parts.push(format!("{} cached", self.cache_hits));
        }
        if self.mounts_skipped > 0 {
            parts.push(format!("{} mounts skipped", self.mounts_skipped));
        }
        if self.watch_enabled {
            parts.push("watching".to_owned());
        }
        if self.active_view != ViewKind::Tree && self.analysis_rows_truncated {
            parts.push(format!("top {} rows", self.analysis_rows.len()));
        }
        if let Some(notice) = &self.notice {
            parts.push(notice.clone());
        }
        parts.join(" | ")
    }

    pub fn is_marked(&self, node_id: NodeId) -> bool {
        self.tree
            .nodes
            .get(node_id)
            .is_some_and(|node| self.marked.contains(&node.path))
    }

    pub fn is_path_marked(&self, path: &Path) -> bool {
        self.marked.contains(path)
    }

    pub fn can_delete_path(&self, path: &Path) -> bool {
        path != self.root && path.starts_with(&self.root)
    }

    pub fn analysis_known_usage(&self) -> crate::tree::UsageStats {
        self.analysis_index
            .as_ref()
            .map(|index| {
                if index.complete {
                    index.root_usage()
                } else {
                    self.analysis_found_usage
                }
            })
            .unwrap_or_default()
    }

    pub fn selected_index(&self) -> Option<usize> {
        let selected = self.selected?;
        self.visible
            .iter()
            .position(|visible| visible.node_id == selected)
    }

    pub fn selected_node(&self) -> Option<&Node> {
        self.selected
            .and_then(|node_id| self.tree.nodes.get(node_id))
    }

    pub fn selected_percentage(&self) -> Option<f64> {
        self.selected.and_then(|node_id| self.percentage(node_id))
    }

    pub fn percentage(&self, node_id: NodeId) -> Option<f64> {
        let node = self.tree.nodes.get(node_id)?;
        let child_size = node.usage?.size(self.size_mode);
        let parent_size = match node.parent {
            Some(parent) if parent == self.tree.root_id => {
                self.tree.root_known_usage().size(self.size_mode)
            }
            Some(parent) => self.tree.nodes.get(parent)?.usage?.size(self.size_mode),
            None => return None,
        };
        (parent_size > 0).then_some(child_size as f64 * 100.0 / parent_size as f64)
    }

    pub fn set_page_size(&mut self, page_size: usize) {
        self.page_size = page_size.max(1);
        if self.active_view == ViewKind::Tree {
            self.ensure_selection_visible();
        } else {
            self.ensure_analysis_selection_visible();
        }
    }

    pub fn set_list_area(&mut self, area: Rect) {
        self.list_area = area;
        self.set_page_size(area.height as usize);
    }

    fn scan_options(&self) -> ScanOptions {
        ScanOptions {
            root_device: self.root_device,
            one_file_system: self.one_file_system,
        }
    }

    fn queue_children(&mut self, node_id: NodeId) -> Result<()> {
        let options = self.scan_options();
        let Some(node) = self.tree.nodes.get_mut(node_id) else {
            return Ok(());
        };
        if node.children_loaded
            || node.children_loading
            || node.kind != NodeKind::Directory
            || node.mountpoint
        {
            return Ok(());
        }
        node.children_loading = true;
        let path = node.path.clone();
        if let Err(error) = self.scan_pool.send(ScanJob::LoadChildren {
            generation: self.generation,
            node_id,
            path,
            options,
        }) {
            node.children_loading = false;
            return Err(error);
        }
        self.pending_jobs += 1;
        Ok(())
    }

    fn queue_size(&mut self, node_id: NodeId) -> Result<()> {
        self.queue_size_with_mode(node_id, false)
    }

    fn rescan_size(&mut self, node_id: NodeId) -> Result<()> {
        self.queue_size_with_mode(node_id, true)
    }

    fn queue_size_with_mode(&mut self, node_id: NodeId, force: bool) -> Result<()> {
        let options = self.scan_options();
        let Some(node) = self.tree.nodes.get_mut(node_id) else {
            return Ok(());
        };
        if node.kind != NodeKind::Directory
            || node.mountpoint
            || (!force && !matches!(node.scan_state, ScanState::NotScanned | ScanState::Error))
        {
            return Ok(());
        }
        node.scan_revision = node
            .scan_revision
            .checked_add(1)
            .ok_or_else(|| anyhow!("Scan revision overflow for {}", node.path.display()))?;
        node.scan_state = ScanState::Queued;
        node.usage = None;
        node.cached = false;
        node.error = None;
        node.warning_count = 0;
        let scan_revision = node.scan_revision;
        let path = node.path.clone();
        if force {
            self.cache.invalidate_path(&path);
        }
        if let Err(error) = self.scan_pool.send(ScanJob::CalculateSize {
            generation: self.generation,
            node_id,
            scan_revision,
            path,
            options,
        }) {
            node.scan_state = ScanState::Error;
            return Err(error);
        }
        self.pending_jobs += 1;
        Ok(())
    }

    fn drain_events(&mut self) {
        for _ in 0..MAX_SCAN_EVENTS_PER_TICK {
            let Ok(event) = self.scan_pool.try_recv() else {
                break;
            };
            self.handle_worker_event(event);
        }
        for _ in 0..MAX_ANALYSIS_EVENTS_PER_TICK {
            let Ok(event) = self.analysis_worker.try_recv() else {
                break;
            };
            self.handle_analysis_event(event);
        }
        for _ in 0..MAX_FILE_EVENTS_PER_TICK {
            let Ok(result) = self.file_worker.try_recv() else {
                break;
            };
            self.handle_delete_result(result);
        }
        let watch_batch = self.watcher.as_mut().and_then(WatchService::poll);
        if let Some(batch) = watch_batch {
            self.handle_watch_batch(batch);
        }
    }

    fn ensure_analysis(&mut self) {
        if self
            .analysis_index
            .as_ref()
            .is_some_and(|index| index.complete)
            || self.analysis_status == AnalysisStatus::Indexing
        {
            if self.active_view == ViewKind::Duplicates {
                self.ensure_duplicates();
            }
            return;
        }
        let index = AnalysisIndex::new(
            self.root.clone(),
            self.one_file_system,
            crate::analytics::now_seconds(),
        );
        self.analysis_index = Some(index);
        self.analysis_status = AnalysisStatus::Indexing;
        self.analysis_rows.clear();
        self.analysis_rows_truncated = false;
        self.analysis_selected = 0;
        self.analysis_scroll = 0;
        self.analysis_found_usage = crate::tree::UsageStats::default();
        if let Err(error) = self.analysis_worker.start_index(
            self.generation,
            self.root.clone(),
            self.scan_options(),
        ) {
            self.analysis_status = AnalysisStatus::Idle;
            self.show_error(error.to_string());
        } else {
            self.notice = Some("Building full analysis index…".to_owned());
        }
    }

    fn refresh_analysis_subtrees(&mut self, mut paths: Vec<PathBuf>) {
        let Some(index) = &mut self.analysis_index else {
            if self.active_view != ViewKind::Tree {
                self.ensure_analysis();
            }
            return;
        };
        if !index.complete {
            self.analysis_index = None;
            self.analysis_refresh_pending.clear();
            self.analysis_previous_usage.clear();
            if self.active_view != ViewKind::Tree {
                self.ensure_analysis();
            }
            return;
        }

        paths.sort_by_key(|path| path.components().count());
        paths.dedup();
        let mut roots = Vec::<PathBuf>::new();
        for path in paths {
            if !roots.iter().any(|root| path.starts_with(root)) {
                roots.push(path);
            }
        }
        if roots.is_empty() {
            return;
        }

        index.complete = false;
        index.duplicates = None;
        self.analysis_found_usage = index.root_usage();
        for path in &roots {
            let previous = index
                .directories
                .iter()
                .find(|directory| directory.path == *path)
                .map(|directory| directory.usage)
                .unwrap_or_default();
            self.analysis_previous_usage.insert(path.clone(), previous);
            self.analysis_found_usage = subtract_usage(self.analysis_found_usage, previous);
            index.remove_subtree(path);
            self.analysis_refresh_pending.insert(path.clone());
        }
        self.analysis_status = AnalysisStatus::Indexing;
        for path in roots {
            if let Err(error) =
                self.analysis_worker
                    .start_index(self.generation, path.clone(), self.scan_options())
            {
                self.analysis_refresh_pending.remove(&path);
                self.analysis_previous_usage.remove(&path);
                self.notice = Some(format!("Could not refresh analysis slice: {error}"));
            }
        }
        if self.analysis_refresh_pending.is_empty() {
            if let Some(index) = &mut self.analysis_index {
                index.complete = true;
            }
            self.analysis_status = AnalysisStatus::Ready;
        } else {
            self.notice = Some(format!(
                "Refreshing {} analysis slice(s)…",
                self.analysis_refresh_pending.len()
            ));
        }
        self.rebuild_analysis_rows();
    }

    fn handle_analysis_event(&mut self, event: AnalysisEvent) {
        let generation = match &event {
            AnalysisEvent::Started { generation, .. }
            | AnalysisEvent::FileBatch { generation, .. }
            | AnalysisEvent::Finished { generation, .. }
            | AnalysisEvent::DuplicatesStarted { generation, .. }
            | AnalysisEvent::DuplicatesFinished { generation, .. } => *generation,
        };
        if generation != self.generation {
            return;
        }
        match event {
            AnalysisEvent::Started {
                scan_root,
                started_at,
                ..
            } => {
                let index = self.analysis_index.get_or_insert_with(|| {
                    AnalysisIndex::new(self.root.clone(), self.one_file_system, started_at)
                });
                if !self.analysis_refresh_pending.contains(&scan_root) {
                    index.started_at = started_at;
                }
                self.analysis_status = AnalysisStatus::Indexing;
            }
            AnalysisEvent::FileBatch { files, .. } => {
                let batch_usage = files
                    .iter()
                    .map(|file| file.usage)
                    .fold(crate::tree::UsageStats::default(), |total, usage| {
                        total.saturating_add(usage)
                    });
                self.analysis_found_usage = self.analysis_found_usage.saturating_add(batch_usage);
                if let Some(index) = &mut self.analysis_index {
                    index.files.extend(files);
                }
            }
            AnalysisEvent::Finished {
                scan_root,
                completed_at,
                directories,
                issues,
                mounts_skipped,
                ..
            } => {
                let incremental = self.analysis_refresh_pending.remove(&scan_root);
                if let Some(index) = &mut self.analysis_index {
                    index.completed_at = Some(completed_at);
                    if incremental {
                        let previous = self
                            .analysis_previous_usage
                            .remove(&scan_root)
                            .unwrap_or_default();
                        let current = directories
                            .iter()
                            .find(|directory| directory.path == scan_root)
                            .map(|directory| directory.usage)
                            .unwrap_or_default();
                        for directory in &mut index.directories {
                            if scan_root.starts_with(&directory.path) && directory.path != scan_root
                            {
                                directory.usage = replace_usage(directory.usage, previous, current);
                            }
                        }
                        index.directories.extend(directories);
                        index
                            .issues
                            .retain(|issue| !issue.path.starts_with(&scan_root));
                        index.issues.extend(issues);
                        index.mounts_skipped = index.mounts_skipped.saturating_add(mounts_skipped);
                        index.complete = self.analysis_refresh_pending.is_empty();
                    } else {
                        index.directories = directories;
                        index.issues = issues;
                        index.mounts_skipped = mounts_skipped;
                        index.complete = true;
                    }
                }
                if !self.analysis_refresh_pending.is_empty() {
                    return;
                }
                self.analysis_found_usage = self
                    .analysis_index
                    .as_ref()
                    .map(AnalysisIndex::root_usage)
                    .unwrap_or_default();
                self.analysis_status = AnalysisStatus::Ready;
                self.finish_analysis_snapshot();
                self.rebuild_analysis_rows();
                if self.detect_duplicates || self.active_view == ViewKind::Duplicates {
                    self.ensure_duplicates();
                } else if let Some(index) = &self.analysis_index {
                    self.notice = Some(format!(
                        "Analysis complete: {} files, {} issue(s)",
                        index.files.len(),
                        index.issues.len()
                    ));
                }
            }
            AnalysisEvent::DuplicatesStarted { candidates, .. } => {
                self.analysis_status = AnalysisStatus::Hashing;
                self.notice = Some(format!("Hashing {candidates} duplicate candidates…"));
            }
            AnalysisEvent::DuplicatesFinished { groups, issues, .. } => {
                if let Some(index) = &mut self.analysis_index {
                    index.duplicates = Some(groups);
                    index.issues.extend(issues);
                }
                self.analysis_status = AnalysisStatus::Ready;
                self.rebuild_analysis_rows();
                let groups = self
                    .analysis_index
                    .as_ref()
                    .and_then(|index| index.duplicates.as_ref())
                    .map_or(0, Vec::len);
                self.notice = Some(format!(
                    "Exact duplicate analysis complete: {groups} group(s)"
                ));
            }
        }
    }

    fn finish_analysis_snapshot(&mut self) {
        let Some(index) = &self.analysis_index else {
            return;
        };
        let rolling = match self.snapshot_store.process_completed(index) {
            Ok(rolling) => rolling,
            Err(error) => {
                self.notice = Some(format!("Snapshot warning: {error}"));
                return;
            }
        };
        if let Some(warning) = rolling.warning {
            self.notice = Some(warning);
        }
        self.comparison = if let Some(path) = &self.compare_snapshot {
            match load_compatible_snapshot(path, index) {
                Ok(previous) => Some(compare_indices(&previous.index, index)),
                Err(error) => {
                    self.notice = Some(format!("Snapshot comparison failed: {error}"));
                    rolling.comparison
                }
            }
        } else {
            rolling.comparison
        };
    }

    fn ensure_duplicates(&mut self) {
        let Some(index) = &self.analysis_index else {
            return;
        };
        if !index.complete
            || index.duplicates.is_some()
            || self.analysis_status == AnalysisStatus::Hashing
        {
            return;
        }
        if let Err(error) = self
            .analysis_worker
            .start_duplicates(self.generation, index.files.clone())
        {
            self.show_error(error.to_string());
        }
    }

    fn set_view(&mut self, view: ViewKind) {
        let changed = self.active_view != view;
        self.active_view = view;
        if view == ViewKind::Tree {
            self.ensure_selection_visible();
        } else {
            self.ensure_analysis();
            if changed
                && self
                    .analysis_index
                    .as_ref()
                    .is_some_and(|index| !index.complete)
            {
                self.analysis_rows.clear();
                self.analysis_rows_truncated = false;
                self.analysis_selected = 0;
                self.analysis_scroll = 0;
            }
            self.rebuild_analysis_rows();
            self.ensure_analysis_selection_visible();
        }
    }

    fn rebuild_analysis_rows(&mut self) {
        if self.active_view == ViewKind::Tree {
            return;
        }
        let Some(index) = &self.analysis_index else {
            self.analysis_rows.clear();
            self.analysis_rows_truncated = false;
            return;
        };
        if !index.complete {
            return;
        }
        let matches = |file: &FileRecord| {
            self.filter_expression
                .as_ref()
                .is_none_or(|filter| filter.matches_file(file, self.size_mode))
        };
        let mut truncated = false;
        let mut rows = match self.active_view {
            ViewKind::Tree => Vec::new(),
            ViewKind::Largest => {
                let mut files = index
                    .files
                    .iter()
                    .filter(|file| matches(file))
                    .collect::<Vec<_>>();
                if files.len() > MAX_ANALYSIS_ROWS {
                    truncated = true;
                    files.select_nth_unstable_by(MAX_ANALYSIS_ROWS, |left, right| {
                        compare_file_records(left, right, self.sort, self.size_mode)
                    });
                    files.truncate(MAX_ANALYSIS_ROWS);
                }
                files.sort_unstable_by(|left, right| {
                    compare_file_records(left, right, self.sort, self.size_mode)
                });
                files
                    .into_iter()
                    .map(|file| AnalysisRow {
                        label: file
                            .path
                            .file_name()
                            .unwrap_or(file.path.as_os_str())
                            .to_string_lossy()
                            .into_owned(),
                        path: Some(file.path.clone()),
                        usage: file.usage,
                        files: 1,
                        detail: format!(
                            "{} · {}",
                            file.category.label(),
                            file.extension.as_deref().unwrap_or("no extension")
                        ),
                        indent: 0,
                    })
                    .collect()
            }
            ViewKind::Types => {
                let summary = index.summary(index.files.iter().filter(|file| matches(file)));
                let mut rows = Vec::new();
                append_group_rows(&mut rows, "Category", summary.categories);
                append_group_rows(&mut rows, "Extension", summary.extensions);
                append_group_rows(&mut rows, "Age", summary.age_buckets);
                rows
            }
            ViewKind::Duplicates => {
                let mut rows = Vec::new();
                for (number, group) in index
                    .duplicates
                    .as_deref()
                    .unwrap_or_default()
                    .iter()
                    .enumerate()
                {
                    let visible_files = group
                        .files
                        .iter()
                        .filter(|entry| {
                            index
                                .files
                                .iter()
                                .find(|file| file.path == entry.path)
                                .is_none_or(&matches)
                        })
                        .collect::<Vec<_>>();
                    if visible_files.is_empty() {
                        continue;
                    }
                    rows.push(AnalysisRow {
                        label: format!(
                            "Group {} · {}/{} copies",
                            number + 1,
                            visible_files.len(),
                            group.files.len()
                        ),
                        path: None,
                        usage: crate::tree::UsageStats {
                            logical: group.reclaimable_logical,
                            physical: group.physical_total,
                            files: group.files.len() as u64,
                        },
                        files: group.files.len() as u64,
                        detail: format!("reclaimable · {}", &group.hash[..12]),
                        indent: 0,
                    });
                    for file in visible_files {
                        rows.push(AnalysisRow {
                            label: file
                                .path
                                .file_name()
                                .unwrap_or(file.path.as_os_str())
                                .to_string_lossy()
                                .into_owned(),
                            path: Some(file.path.clone()),
                            usage: file.usage,
                            files: 1,
                            detail: if file.hardlink_aliases.is_empty() {
                                "exact duplicate".to_owned()
                            } else {
                                format!("{} hardlink alias(es)", file.hardlink_aliases.len())
                            },
                            indent: 1,
                        });
                    }
                }
                rows
            }
            ViewKind::Changes => self
                .comparison
                .as_ref()
                .into_iter()
                .flat_map(|comparison| &comparison.changes)
                .filter(|change| {
                    let usage = change.current.or(change.previous).unwrap_or_default();
                    if let Some(filter) = &self.filter_expression {
                        let modified_seconds = index
                            .files
                            .iter()
                            .find(|file| file.path == change.path)
                            .map(|file| file.modified_seconds);
                        filter.matches_path_usage(
                            &change.path,
                            usage,
                            modified_seconds,
                            self.size_mode,
                        )
                    } else {
                        self.filter_query.as_ref().is_none_or(|query| {
                            change
                                .path
                                .to_string_lossy()
                                .to_lowercase()
                                .contains(&query.to_lowercase())
                        })
                    }
                })
                .map(|change| AnalysisRow {
                    label: change
                        .path
                        .file_name()
                        .unwrap_or(change.path.as_os_str())
                        .to_string_lossy()
                        .into_owned(),
                    path: Some(change.path.clone()),
                    usage: change.current.or(change.previous).unwrap_or_default(),
                    files: change
                        .current
                        .or(change.previous)
                        .map_or(0, |usage| usage.files),
                    detail: format!(
                        "{} {} · {:+} logical bytes",
                        change.kind.label(),
                        change.object.label(),
                        change.logical_delta
                    ),
                    indent: 0,
                })
                .collect(),
        };
        if matches!(self.active_view, ViewKind::Types | ViewKind::Changes) {
            rows.sort_by(|left, right| {
                compare_analysis_rows(left, right, self.sort, self.size_mode)
            });
        }
        if rows.len() > MAX_ANALYSIS_ROWS {
            rows.truncate(MAX_ANALYSIS_ROWS);
            truncated = true;
        }
        self.analysis_rows_truncated = truncated;
        self.analysis_rows = rows;
        self.analysis_selected = self
            .analysis_selected
            .min(self.analysis_rows.len().saturating_sub(1));
        self.ensure_analysis_selection_visible();
    }

    fn ensure_analysis_selection_visible(&mut self) {
        if self.analysis_rows.is_empty() {
            self.analysis_selected = 0;
            self.analysis_scroll = 0;
            return;
        }
        if self.analysis_selected < self.analysis_scroll {
            self.analysis_scroll = self.analysis_selected;
        } else if self.analysis_selected >= self.analysis_scroll.saturating_add(self.page_size) {
            self.analysis_scroll = self.analysis_selected + 1 - self.page_size;
        }
        self.analysis_scroll = self
            .analysis_scroll
            .min(self.analysis_rows.len().saturating_sub(self.page_size));
    }

    fn handle_worker_event(&mut self, event: WorkerEvent) {
        let event_generation = match &event {
            WorkerEvent::SizeStarted { generation, .. }
            | WorkerEvent::ChildrenLoaded { generation, .. }
            | WorkerEvent::ChildrenLoadFailed { generation, .. }
            | WorkerEvent::SizeCalculated { generation, .. } => *generation,
        };
        if event_generation != self.generation {
            return;
        }

        match event {
            WorkerEvent::SizeStarted {
                node_id,
                scan_revision,
                ..
            } => {
                if let Some(node) = self.tree.nodes.get_mut(node_id)
                    && node.scan_revision == scan_revision
                    && node.scan_state == ScanState::Queued
                {
                    node.scan_state = ScanState::Scanning;
                }
            }
            WorkerEvent::ChildrenLoaded {
                node_id, outcome, ..
            } => {
                self.finish_job();
                if !self.tree.nodes.contains_key(node_id) {
                    return;
                }
                self.record_warnings(
                    outcome.warnings.error_count,
                    outcome.warnings.first_message.clone(),
                );
                if let Some(node) = self.tree.nodes.get_mut(node_id) {
                    node.children_loading = false;
                    node.children_loaded = true;
                    node.warning_count = node
                        .warning_count
                        .saturating_add(outcome.warnings.error_count);
                    if node.error.is_none() {
                        node.error = outcome.warnings.first_message;
                    }
                }

                let mut directories = Vec::new();
                for entry in outcome.entries {
                    if entry.mountpoint {
                        self.mounts_skipped = self.mounts_skipped.saturating_add(1);
                    }
                    let Some(child) = self.tree.add_child(node_id, entry) else {
                        continue;
                    };
                    if self.tree.nodes[child].kind == NodeKind::Directory
                        && !self.tree.nodes[child].mountpoint
                    {
                        let cache_hit = self.tree.nodes[child].identity.and_then(|identity| {
                            self.cache.lookup(
                                &self.tree.nodes[child].path,
                                identity,
                                self.one_file_system,
                            )
                        });
                        if let Some(usage) = cache_hit {
                            let node = &mut self.tree.nodes[child];
                            node.usage = Some(usage);
                            node.scan_state = ScanState::Complete;
                            node.cached = true;
                            self.cache_hits = self.cache_hits.saturating_add(1);
                            self.sort_dirty = true;
                        } else {
                            directories.push(child);
                        }
                    }
                }
                self.tree.sort_children(node_id, self.sort, self.size_mode);
                for directory in directories {
                    if let Err(error) = self.queue_size(directory) {
                        self.show_error(error.to_string());
                    }
                }
                self.rebuild_visible();
                self.apply_pending_selection();
            }
            WorkerEvent::ChildrenLoadFailed {
                node_id, message, ..
            } => {
                self.finish_job();
                self.record_warnings(1, Some(message.clone()));
                if let Some(node) = self.tree.nodes.get_mut(node_id) {
                    node.children_loading = false;
                    node.error = Some(message);
                    node.warning_count = node.warning_count.saturating_add(1);
                    if node_id == self.tree.root_id {
                        node.scan_state = ScanState::Error;
                    }
                }
                self.rebuild_visible();
            }
            WorkerEvent::SizeCalculated {
                node_id,
                scan_revision,
                outcome,
                ..
            } => {
                self.finish_job();
                if self
                    .tree
                    .nodes
                    .get(node_id)
                    .is_none_or(|node| node.scan_revision != scan_revision)
                {
                    return;
                }
                self.record_warnings(
                    outcome.warnings.error_count,
                    outcome.warnings.first_message.clone(),
                );
                self.mounts_skipped = self.mounts_skipped.saturating_add(outcome.mounts_skipped);
                let mut cache_value = None;
                if let Some(node) = self.tree.nodes.get_mut(node_id) {
                    node.warning_count = outcome.warnings.error_count;
                    node.error = outcome
                        .fatal_error
                        .clone()
                        .or(outcome.warnings.first_message);
                    if outcome.fatal_error.is_some() {
                        node.usage = None;
                        node.scan_state = ScanState::Error;
                    } else {
                        node.usage = Some(outcome.usage);
                        node.scan_state = ScanState::Complete;
                        node.cached = false;
                        if let Some(identity) = node.identity {
                            cache_value = Some((node.path.clone(), identity, outcome.usage));
                        }
                    }
                }
                if let Some((path, identity, usage)) = cache_value {
                    self.cache
                        .insert(&path, identity, usage, self.one_file_system);
                }
                self.sort_dirty = true;
            }
        }
    }

    fn finish_job(&mut self) {
        self.pending_jobs = self.pending_jobs.saturating_sub(1);
    }

    fn record_warnings(&mut self, count: usize, first_message: Option<String>) {
        self.errors = self.errors.saturating_add(count);
        if count > 0
            && let Some(message) = first_message
        {
            self.notice = Some(message);
        }
    }

    fn maybe_sort(&mut self) {
        if !self.sort_dirty {
            return;
        }
        if self.pending_jobs > 0 && self.last_sort.elapsed() < SORT_INTERVAL {
            return;
        }
        self.tree.sort_all_loaded(self.sort, self.size_mode);
        self.sort_dirty = false;
        self.last_sort = Instant::now();
        self.rebuild_visible();
    }

    fn maybe_flush_cache(&mut self) {
        if self.last_cache_flush.elapsed() < CACHE_FLUSH_INTERVAL {
            return;
        }
        if let Err(error) = self.cache.flush() {
            self.notice = Some(error);
        }
        self.last_cache_flush = Instant::now();
    }

    fn rebuild_visible(&mut self) {
        let previous = self.selected;
        self.visible = if let Some(filter) = &self.filter_expression {
            self.tree
                .flatten_visible_matching(Some(&|node| filter.matches_node(node, self.size_mode)))
        } else {
            self.tree
                .flatten_visible_filtered(self.filter_query.as_deref())
        };
        self.selected = previous
            .filter(|selected| {
                self.visible
                    .iter()
                    .any(|visible| visible.node_id == *selected)
            })
            .or_else(|| self.visible.first().map(|visible| visible.node_id));
        self.ensure_selection_visible();
    }

    fn ensure_selection_visible(&mut self) {
        let Some(index) = self.selected_index() else {
            self.scroll_offset = 0;
            return;
        };
        if index < self.scroll_offset {
            self.scroll_offset = index;
        } else if index >= self.scroll_offset.saturating_add(self.page_size) {
            self.scroll_offset = index + 1 - self.page_size;
        }
        let max_offset = self.visible.len().saturating_sub(self.page_size);
        self.scroll_offset = self.scroll_offset.min(max_offset);
    }

    fn select_index(&mut self, index: usize) {
        if self.active_view != ViewKind::Tree {
            if !self.analysis_rows.is_empty() {
                self.analysis_selected = index.min(self.analysis_rows.len() - 1);
                self.ensure_analysis_selection_visible();
            }
            return;
        }
        if let Some(visible) = self
            .visible
            .get(index.min(self.visible.len().saturating_sub(1)))
        {
            self.selected = Some(visible.node_id);
            self.ensure_selection_visible();
        }
    }

    fn move_selection(&mut self, delta: isize) {
        if self.active_view != ViewKind::Tree {
            if self.analysis_rows.is_empty() {
                return;
            }
            self.analysis_selected = self
                .analysis_selected
                .saturating_add_signed(delta)
                .min(self.analysis_rows.len() - 1);
            self.ensure_analysis_selection_visible();
            return;
        }
        if self.visible.is_empty() {
            return;
        }
        let current = self.selected_index().unwrap_or(0);
        let next = current
            .saturating_add_signed(delta)
            .min(self.visible.len() - 1);
        self.select_index(next);
    }

    fn handle_key(&mut self, key: KeyEvent) {
        match &self.mode {
            AppMode::Browse => self.handle_browse_key(key),
            AppMode::Help => self.handle_help_key(key),
            AppMode::Input { .. } => self.handle_prompt_key(key),
            AppMode::ConfirmDelete { items, multi } => match key.code {
                KeyCode::Char('y') | KeyCode::Char('Y') => {
                    let items = items.clone();
                    let multi = *multi;
                    self.start_delete(items, multi);
                }
                KeyCode::Char('n') | KeyCode::Char('N') | KeyCode::Esc => {
                    self.mode = AppMode::Browse;
                    self.notice = Some("Delete cancelled".to_owned());
                }
                _ => {}
            },
            AppMode::Deleting { .. } => {
                if key.code == KeyCode::Char('q')
                    || (key.code == KeyCode::Char('c')
                        && key.modifiers.contains(KeyModifiers::CONTROL))
                {
                    self.quit_after_delete = true;
                    self.notice = Some("Waiting for Trash operation before quitting".to_owned());
                }
            }
            AppMode::ErrorDialog(_) => {
                if matches!(key.code, KeyCode::Esc | KeyCode::Enter) {
                    self.mode = AppMode::Browse;
                }
            }
        }
    }

    fn handle_browse_key(&mut self, key: KeyEvent) {
        match key.code {
            KeyCode::Char('q') => self.should_quit = true,
            KeyCode::Char('c') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                self.should_quit = true;
            }
            KeyCode::Up | KeyCode::Char('k') => self.move_selection(-1),
            KeyCode::Down | KeyCode::Char('j') => self.move_selection(1),
            KeyCode::Home | KeyCode::Char('g') => self.select_index(0),
            KeyCode::End | KeyCode::Char('G') => {
                self.select_index(self.active_row_count().saturating_sub(1))
            }
            KeyCode::PageUp => self.move_selection(-(self.page_size as isize)),
            KeyCode::PageDown => self.move_selection(self.page_size as isize),
            KeyCode::Right | KeyCode::Char('l') if self.active_view == ViewKind::Tree => {
                self.expand_selected();
            }
            KeyCode::Left | KeyCode::Char('h') if self.active_view == ViewKind::Tree => {
                self.collapse_selected();
            }
            KeyCode::Enter if self.active_view == ViewKind::Tree => self.toggle_selected(),
            KeyCode::Enter => self.reveal_analysis_selection(),
            KeyCode::Tab => self.set_view(self.active_view.next(false)),
            KeyCode::BackTab => self.set_view(self.active_view.next(true)),
            KeyCode::Char('1') => self.set_view(ViewKind::Tree),
            KeyCode::Char('2') => self.set_view(ViewKind::Largest),
            KeyCode::Char('3') => self.set_view(ViewKind::Types),
            KeyCode::Char('4') => self.set_view(ViewKind::Duplicates),
            KeyCode::Char('5') => self.set_view(ViewKind::Changes),
            KeyCode::Char('?') => {
                self.help_scroll = 0;
                self.mode = AppMode::Help;
            }
            KeyCode::Char('i') => {
                self.detail_panel = !self.detail_panel;
                self.notice = Some(
                    if self.detail_panel {
                        "Detail panel enabled"
                    } else {
                        "Detail panel hidden"
                    }
                    .to_owned(),
                );
            }
            KeyCode::Char('z') => {
                self.size_mode = self.size_mode.toggled();
                self.sort_dirty = true;
                self.rebuild_analysis_rows();
                self.notice = Some(format!("Showing {} sizes", self.size_mode.label()));
            }
            KeyCode::Esc => self.cancel_scan(),
            KeyCode::Backspace => self.navigate_parent(),
            KeyCode::Char('/') => self.open_prompt(PromptKind::Search),
            KeyCode::Char('n') => self.find_search_match(1, true),
            KeyCode::Char('N') => self.find_search_match(-1, true),
            KeyCode::Char('f') => self.open_prompt(PromptKind::Filter),
            KeyCode::Char('F') => {
                self.filter_query = None;
                self.filter_expression = None;
                self.rebuild_visible();
                self.rebuild_analysis_rows();
                self.notice = Some("Filter cleared".to_owned());
            }
            KeyCode::Char('s') => self.cycle_sort_key(),
            KeyCode::Char('S') => {
                self.sort.direction = self.sort.direction.reversed();
                self.apply_sort_notice();
            }
            KeyCode::Char('t') => {
                self.theme = self.theme.next();
                self.notice = Some(format!("Theme: {}", self.theme.label()));
            }
            KeyCode::Char('m') => {
                self.mouse_enabled = !self.mouse_enabled;
                self.notice = Some(
                    if self.mouse_enabled {
                        "Mouse capture enabled"
                    } else {
                        "Mouse capture disabled"
                    }
                    .to_owned(),
                );
            }
            KeyCode::Char('w') => self.toggle_watcher(),
            KeyCode::Char('e') => self.open_prompt(PromptKind::Export),
            KeyCode::Char('d') => self.confirm_current_delete(),
            KeyCode::Char(' ') => self.toggle_mark_current(),
            KeyCode::Char('x') => self.confirm_marked_delete(),
            KeyCode::Char('r') => self.refresh_root(),
            _ => {}
        }
    }

    fn handle_help_key(&mut self, key: KeyEvent) {
        match key.code {
            KeyCode::Esc | KeyCode::Char('?') | KeyCode::Char('q') => {
                self.mode = AppMode::Browse;
            }
            KeyCode::Up | KeyCode::Char('k') => {
                self.help_scroll = self.help_scroll.saturating_sub(1);
            }
            KeyCode::Down | KeyCode::Char('j') => {
                self.help_scroll = self.help_scroll.saturating_add(1);
            }
            KeyCode::PageUp => {
                self.help_scroll = self.help_scroll.saturating_sub(self.page_size);
            }
            KeyCode::PageDown => {
                self.help_scroll = self.help_scroll.saturating_add(self.page_size);
            }
            _ => {}
        }
    }

    fn open_prompt(&mut self, kind: PromptKind) {
        let value = match kind {
            PromptKind::Search => self.search_query.clone().unwrap_or_default(),
            PromptKind::Filter => self.filter_query.clone().unwrap_or_default(),
            PromptKind::Export => "macDirStat-snapshot.json".to_owned(),
        };
        self.mode = AppMode::Input {
            kind,
            value,
            previous_filter: self.filter_query.clone(),
        };
    }

    fn handle_prompt_key(&mut self, key: KeyEvent) {
        let mut filter_changed = false;
        let mut commit_search = false;
        let mut cancel_filter = None;
        let mut close = false;
        let mut export_path = None;
        let mut filter_error = None;
        if let AppMode::Input {
            kind,
            value,
            previous_filter,
        } = &mut self.mode
        {
            match key.code {
                KeyCode::Esc => {
                    if *kind == PromptKind::Filter {
                        cancel_filter = Some(previous_filter.clone());
                    }
                    close = true;
                }
                KeyCode::Enter => {
                    match kind {
                        PromptKind::Search => {
                            self.search_query = normalize_query(value);
                            commit_search = true;
                        }
                        PromptKind::Filter => match FilterExpression::parse(value) {
                            Ok(expression) => {
                                self.filter_query = normalize_query(value);
                                self.filter_expression = expression;
                                filter_changed = true;
                            }
                            Err(error) => {
                                filter_error = Some(error.to_string());
                            }
                        },
                        PromptKind::Export => export_path = normalize_query(value),
                    }
                    close = filter_error.is_none();
                }
                KeyCode::Backspace => {
                    value.pop();
                    if *kind == PromptKind::Filter {
                        self.filter_query = normalize_query(value);
                        filter_changed = true;
                    }
                }
                KeyCode::Char(character)
                    if !key
                        .modifiers
                        .intersects(KeyModifiers::CONTROL | KeyModifiers::ALT) =>
                {
                    value.push(character);
                    if *kind == PromptKind::Filter {
                        self.filter_query = normalize_query(value);
                        filter_changed = true;
                    }
                }
                _ => {}
            }
        }
        if let Some(previous) = cancel_filter {
            self.filter_query = previous;
            self.filter_expression = self
                .filter_query
                .as_deref()
                .and_then(|value| FilterExpression::parse(value).ok().flatten());
            filter_changed = true;
        }
        if close {
            self.mode = AppMode::Browse;
        }
        if filter_changed {
            if filter_error.is_none() {
                let expression = self
                    .filter_query
                    .as_deref()
                    .map(FilterExpression::parse)
                    .transpose();
                let Ok(expression) = expression else {
                    return;
                };
                self.filter_expression = expression.flatten();
            }
            self.rebuild_visible();
            self.rebuild_analysis_rows();
        }
        if commit_search {
            self.find_search_match(1, false);
        }
        if let Some(error) = filter_error {
            self.notice = Some(format!("Invalid filter: {error}"));
        }
        if let Some(path) = export_path {
            self.export_analysis(Path::new(&path));
        }
    }

    fn find_search_match(&mut self, direction: isize, advance_from_current: bool) {
        let Some(query) = self
            .search_query
            .as_deref()
            .filter(|query| !query.is_empty())
        else {
            self.notice = Some("No search query; press / to enter one".to_owned());
            return;
        };
        if self.active_view != ViewKind::Tree {
            let matches: Vec<_> = self
                .analysis_rows
                .iter()
                .enumerate()
                .filter_map(|(index, row)| {
                    let matches = row.label.to_lowercase().contains(&query.to_lowercase())
                        || row.path.as_ref().is_some_and(|path| {
                            path.to_string_lossy()
                                .to_lowercase()
                                .contains(&query.to_lowercase())
                        });
                    matches.then_some(index)
                })
                .collect();
            if matches.is_empty() {
                self.notice = Some(format!("No analysis row matches \"{query}\""));
                return;
            }
            let current = matches
                .iter()
                .position(|index| *index == self.analysis_selected);
            let match_index = if !advance_from_current {
                0
            } else if direction >= 0 {
                current.map_or(0, |index| (index + 1) % matches.len())
            } else {
                current.map_or(matches.len() - 1, |index| {
                    index.checked_sub(1).unwrap_or(matches.len() - 1)
                })
            };
            self.analysis_selected = matches[match_index];
            self.ensure_analysis_selection_visible();
            self.notice = Some(format!(
                "Search match {}/{}",
                match_index + 1,
                matches.len()
            ));
            return;
        }
        let candidates: Vec<_> = if self.filter_query.is_some() {
            self.visible.iter().map(|visible| visible.node_id).collect()
        } else {
            self.tree.all_loaded()
        };
        let matches: Vec<_> = candidates
            .into_iter()
            .filter(|node_id| {
                self.tree
                    .nodes
                    .get(*node_id)
                    .is_some_and(|node| node.matches(query))
            })
            .collect();
        if matches.is_empty() {
            self.notice = Some(format!("No loaded item matches \"{query}\""));
            return;
        }
        let current = self
            .selected
            .and_then(|selected| matches.iter().position(|node_id| *node_id == selected));
        let index = if !advance_from_current {
            0
        } else if direction >= 0 {
            current.map_or(0, |index| (index + 1) % matches.len())
        } else {
            current.map_or(matches.len() - 1, |index| {
                index.checked_sub(1).unwrap_or(matches.len() - 1)
            })
        };
        let node_id = matches[index];
        self.tree.expand_ancestors(node_id);
        self.rebuild_visible();
        if self
            .visible
            .iter()
            .any(|visible| visible.node_id == node_id)
        {
            self.selected = Some(node_id);
            self.ensure_selection_visible();
            self.notice = Some(format!("Search match {}/{}", index + 1, matches.len()));
        }
    }

    fn cycle_sort_key(&mut self) {
        self.sort.key = self.sort.key.next();
        self.sort.direction = match self.sort.key {
            SortKey::Name | SortKey::Kind => SortDirection::Ascending,
            SortKey::Size | SortKey::Files => SortDirection::Descending,
        };
        self.apply_sort_notice();
    }

    fn apply_sort_notice(&mut self) {
        self.tree.sort_all_loaded(self.sort, self.size_mode);
        self.rebuild_visible();
        self.rebuild_analysis_rows();
        self.notice = Some(format!(
            "Sorted by {} {}",
            self.sort.key.label(),
            self.sort.direction.symbol()
        ));
    }

    fn cancel_scan(&mut self) {
        let analysis_active = matches!(
            self.analysis_status,
            AnalysisStatus::Indexing | AnalysisStatus::Hashing
        );
        if self.pending_jobs == 0 && !analysis_active {
            self.notice = Some("No active scan".to_owned());
            return;
        }
        if let Err(error) = self.advance_generation() {
            self.show_error(error.to_string());
            return;
        }
        let cancelled = self.pending_jobs;
        self.pending_jobs = 0;
        for (_, node) in &mut self.tree.nodes {
            node.children_loading = false;
            if matches!(node.scan_state, ScanState::Queued | ScanState::Scanning) {
                node.scan_state = ScanState::NotScanned;
                node.usage = None;
                node.cached = false;
            }
        }
        self.sort_dirty = false;
        if analysis_active {
            self.analysis_status = if self
                .analysis_index
                .as_ref()
                .is_some_and(|index| index.complete)
            {
                AnalysisStatus::Ready
            } else {
                AnalysisStatus::Idle
            };
        }
        self.notice = Some(format!(
            "Cancelled {cancelled} tree job(s){}",
            if analysis_active {
                " and full analysis"
            } else {
                ""
            }
        ));
        self.rebuild_visible();
    }

    fn navigate_parent(&mut self) {
        let Some(parent) = self.root.parent().map(Path::to_path_buf) else {
            self.notice = Some("Already at filesystem root".to_owned());
            return;
        };
        let old_root = self.root.clone();
        match fs::canonicalize(parent) {
            Ok(parent) => {
                if let Err(error) = self.switch_root(
                    parent,
                    Some(old_root),
                    Some("Opened parent directory".to_owned()),
                ) {
                    self.show_error(error.to_string());
                }
            }
            Err(error) => self.show_error(format!("Could not open parent directory: {error}")),
        }
    }

    fn refresh_root(&mut self) {
        let root = self.root.clone();
        self.cache.invalidate_subtree(&root);
        if let Err(error) = self.switch_root(
            root,
            None,
            Some("Refreshing current root without cache".to_owned()),
        ) {
            self.show_error(error.to_string());
        }
    }

    fn switch_root(
        &mut self,
        root: PathBuf,
        pending_select_path: Option<PathBuf>,
        notice: Option<String>,
    ) -> Result<()> {
        self.advance_generation()?;
        self.root_device = root_device(&root)?;
        self.root = root.clone();
        self.tree = Tree::new(root.clone());
        self.visible.clear();
        self.selected = None;
        self.scroll_offset = 0;
        self.pending_jobs = 0;
        self.errors = 0;
        self.notice = notice;
        self.marked.clear();
        self.analysis_index = None;
        self.analysis_rows.clear();
        self.analysis_rows_truncated = false;
        self.analysis_selected = 0;
        self.analysis_scroll = 0;
        self.analysis_status = AnalysisStatus::Idle;
        self.comparison = None;
        self.analysis_refresh_pending.clear();
        self.analysis_previous_usage.clear();
        self.mode = AppMode::Browse;
        self.sort_dirty = false;
        self.last_sort = Instant::now();
        self.cache_hits = 0;
        self.mounts_skipped = 0;
        self.filter_query = None;
        self.search_query = None;
        self.pending_select_path = pending_select_path;
        self.watcher = if self.watch_enabled {
            Some(WatchService::start(root.clone(), self.watch_debounce)?)
        } else {
            None
        };
        self.queue_children(self.tree.root_id)?;
        if self.active_view != ViewKind::Tree {
            self.ensure_analysis();
        }
        Ok(())
    }

    fn advance_generation(&mut self) -> Result<()> {
        self.generation = self
            .generation
            .checked_add(1)
            .ok_or_else(|| anyhow!("Scan generation overflow"))?;
        self.scan_pool.set_generation(self.generation);
        self.analysis_worker.set_generation(self.generation);
        self.analysis_worker.discard_pending();
        Ok(())
    }

    fn apply_pending_selection(&mut self) {
        let Some(path) = self.pending_select_path.clone() else {
            return;
        };
        if let Some((node_id, _)) = self.tree.nodes.iter().find(|(_, node)| node.path == path) {
            self.selected = Some(node_id);
            self.tree.expand_ancestors(node_id);
            self.pending_select_path = None;
            self.rebuild_visible();
            self.ensure_selection_visible();
            return;
        }
        let deepest = self
            .tree
            .nodes
            .iter()
            .filter(|(_, node)| node.kind == NodeKind::Directory && path.starts_with(&node.path))
            .max_by_key(|(_, node)| node.path.components().count())
            .map(|(node_id, _)| node_id);
        if let Some(node_id) = deepest {
            let can_load = self.tree.nodes.get_mut(node_id).is_some_and(|node| {
                node.expanded = true;
                !node.children_loaded && !node.children_loading
            });
            if can_load && let Err(error) = self.queue_children(node_id) {
                self.show_error(error.to_string());
            }
        }
    }

    fn handle_mouse(&mut self, mouse: MouseEvent) {
        if !matches!(self.mode, AppMode::Browse) {
            return;
        }
        match mouse.kind {
            MouseEventKind::ScrollUp => self.move_selection(-3),
            MouseEventKind::ScrollDown => self.move_selection(3),
            MouseEventKind::Down(MouseButton::Left)
                if self.list_area.contains((mouse.column, mouse.row).into()) =>
            {
                let row = usize::from(mouse.row.saturating_sub(self.list_area.y));
                let index = self.scroll_offset.saturating_add(row);
                if self.active_view != ViewKind::Tree {
                    let index = self.analysis_scroll.saturating_add(row);
                    if index < self.analysis_rows.len() {
                        self.analysis_selected = index;
                        self.ensure_analysis_selection_visible();
                        if mouse.column < self.list_area.x.saturating_add(4) {
                            self.toggle_mark_current();
                        }
                    }
                    return;
                }
                let Some(node_id) = self.visible.get(index).map(|visible| visible.node_id) else {
                    return;
                };
                self.selected = Some(node_id);
                self.ensure_selection_visible();
                if mouse.column < self.list_area.x.saturating_add(4) {
                    self.toggle_mark_selected();
                    self.last_click = None;
                    return;
                }
                let now = Instant::now();
                let double_click = self.last_click.is_some_and(|(previous, at)| {
                    previous == node_id && now.duration_since(at) <= DOUBLE_CLICK_INTERVAL
                });
                self.last_click = Some((node_id, now));
                if double_click {
                    self.toggle_selected();
                    self.last_click = None;
                }
            }
            _ => {}
        }
    }

    fn active_row_count(&self) -> usize {
        if self.active_view == ViewKind::Tree {
            self.visible.len()
        } else {
            self.analysis_rows.len()
        }
    }

    pub fn selected_analysis_row(&self) -> Option<&AnalysisRow> {
        (self.active_view != ViewKind::Tree)
            .then(|| self.analysis_rows.get(self.analysis_selected))
            .flatten()
    }

    fn current_delete_item(&self) -> Option<DeleteItem> {
        if self.active_view == ViewKind::Tree {
            let node_id = self.selected?;
            let node = self.tree.nodes.get(node_id)?;
            Some(DeleteItem {
                node_id: Some(node_id),
                path: node.path.clone(),
            })
        } else {
            let path = self.selected_analysis_row()?.path.clone()?;
            if !self.can_delete_path(&path) {
                return None;
            }
            Some(DeleteItem {
                node_id: None,
                path,
            })
        }
    }

    fn toggle_mark_current(&mut self) {
        if self.active_view == ViewKind::Tree {
            self.toggle_mark_selected();
            return;
        }
        let Some(path) = self
            .selected_analysis_row()
            .and_then(|row| row.path.clone())
        else {
            self.notice = Some("This summary row cannot be marked".to_owned());
            return;
        };
        if !self.can_delete_path(&path) {
            self.notice = Some("The analysis root cannot be marked".to_owned());
            return;
        }
        if self.marked.remove(&path) {
            self.notice = Some("Item unmarked".to_owned());
            return;
        }
        if self.has_marked_ancestor(&path) {
            self.notice = Some("Item is already included by a marked parent".to_owned());
            return;
        }
        self.marked
            .retain(|marked| !marked.starts_with(&path) || marked == &path);
        self.marked.insert(path);
        self.notice = Some("Item marked".to_owned());
    }

    fn confirm_current_delete(&mut self) {
        let Some(item) = self.current_delete_item() else {
            self.notice = Some("This row has no deletable path".to_owned());
            return;
        };
        self.mode = AppMode::ConfirmDelete {
            items: vec![item],
            multi: false,
        };
    }

    fn reveal_analysis_selection(&mut self) {
        let Some(path) = self
            .selected_analysis_row()
            .and_then(|row| row.path.clone())
        else {
            self.notice = Some("This summary row has no path to reveal".to_owned());
            return;
        };
        if !path.starts_with(&self.root) {
            self.notice = Some("The selected path is no longer under the current root".to_owned());
            return;
        }
        self.pending_select_path = Some(path);
        self.set_view(ViewKind::Tree);
        self.apply_pending_selection();
    }

    fn export_analysis(&mut self, path: &Path) {
        let Some(index) = self.analysis_index.as_ref().filter(|index| index.complete) else {
            self.notice = Some(
                "A complete analysis is required; building it now, then press e again".to_owned(),
            );
            self.ensure_analysis();
            return;
        };
        match write_export(path, index, self.comparison.clone()) {
            Ok(()) => self.notice = Some(format!("Exported {}", path.display())),
            Err(error) => self.show_error(format!("Export failed: {error}")),
        }
    }

    fn toggle_watcher(&mut self) {
        if self.watch_enabled {
            self.watcher = None;
            self.watch_enabled = false;
            self.notice = Some("Filesystem watcher disabled".to_owned());
            return;
        }
        match WatchService::start(self.root.clone(), self.watch_debounce) {
            Ok(watcher) => {
                self.watcher = Some(watcher);
                self.watch_enabled = true;
                self.notice = Some("Filesystem watcher enabled".to_owned());
            }
            Err(error) => self.show_error(error.to_string()),
        }
    }

    fn handle_watch_batch(&mut self, batch: crate::watcher::WatchBatch) {
        let warning = batch.warning;
        if batch.full_rescan {
            self.refresh_root();
            if let Some(warning) = warning {
                self.notice = Some(warning);
            }
            return;
        }
        if let Some(warning) = warning {
            self.notice = Some(warning);
        }
        if batch.paths.is_empty() {
            return;
        }
        if let Err(error) = self.advance_generation() {
            self.show_error(error.to_string());
            return;
        }
        self.pending_jobs = 0;
        for (_, node) in &mut self.tree.nodes {
            node.children_loading = false;
            if matches!(node.scan_state, ScanState::Queued | ScanState::Scanning) {
                node.scan_state = ScanState::NotScanned;
            }
        }
        let selected_path = self.selected_node().map(|node| node.path.clone());
        let mut reload = HashSet::new();
        let mut rescan = HashSet::new();
        for path in &batch.paths {
            self.cache.invalidate_subtree(path);
            if fs::symlink_metadata(path)
                .is_err_and(|error| error.kind() == io::ErrorKind::NotFound)
            {
                self.marked.retain(|marked| !marked.starts_with(path));
            }
            let directory = if path.is_dir() {
                path.as_path()
            } else {
                path.parent().unwrap_or(&self.root)
            };
            let deepest = self
                .tree
                .nodes
                .iter()
                .filter(|(_, node)| {
                    node.kind == NodeKind::Directory && directory.starts_with(&node.path)
                })
                .max_by_key(|(_, node)| node.path.components().count())
                .map(|(node_id, _)| node_id);
            if let Some(node_id) = deepest {
                reload.insert(node_id);
                let mut current = Some(node_id);
                while let Some(id) = current {
                    if id != self.tree.root_id {
                        rescan.insert(id);
                    }
                    current = self.tree.nodes.get(id).and_then(|node| node.parent);
                }
            }
        }
        let analysis_paths = reload
            .iter()
            .filter_map(|node_id| self.tree.nodes.get(*node_id))
            .map(|node| node.path.clone())
            .collect::<Vec<_>>();
        for node_id in reload {
            if !self.tree.nodes.contains_key(node_id) {
                continue;
            }
            let children = self.tree.nodes[node_id].children.clone();
            for child in children {
                self.tree.remove_subtree(child);
            }
            if let Some(node) = self.tree.nodes.get_mut(node_id) {
                node.children.clear();
                node.children_loaded = false;
                node.children_loading = false;
            }
            if let Err(error) = self.queue_children(node_id) {
                self.notice = Some(error.to_string());
            }
        }
        for node_id in rescan {
            if let Err(error) = self.rescan_size(node_id) {
                self.notice = Some(error.to_string());
            }
        }
        self.pending_select_path = selected_path;
        self.comparison = None;
        self.refresh_analysis_subtrees(analysis_paths);
        self.rebuild_visible();
        self.notice = Some(format!(
            "Applied {} filesystem change(s)",
            batch.paths.len()
        ));
    }

    fn expand_selected(&mut self) {
        let Some(node_id) = self.selected else {
            return;
        };
        let (should_load, should_scan, mountpoint) =
            self.tree
                .nodes
                .get_mut(node_id)
                .map_or((false, false, false), |node| {
                    if node.kind != NodeKind::Directory {
                        return (false, false, false);
                    }
                    if node.mountpoint {
                        return (false, false, true);
                    }
                    node.expanded = true;
                    (
                        !node.children_loaded && !node.children_loading,
                        matches!(node.scan_state, ScanState::NotScanned | ScanState::Error),
                        false,
                    )
                });
        if mountpoint {
            self.notice =
                Some("Mount point skipped; use --cross-filesystems to scan it".to_owned());
            return;
        }
        if should_load && let Err(error) = self.queue_children(node_id) {
            self.show_error(error.to_string());
        }
        if should_scan && let Err(error) = self.queue_size(node_id) {
            self.show_error(error.to_string());
        }
        self.rebuild_visible();
        self.rebuild_analysis_rows();
    }

    fn collapse_selected(&mut self) {
        let Some(node_id) = self.selected else {
            return;
        };
        let (should_rebuild, parent) =
            self.tree
                .nodes
                .get_mut(node_id)
                .map_or((false, None), |node| {
                    if node.kind == NodeKind::Directory && node.expanded {
                        node.expanded = false;
                        (true, None)
                    } else {
                        (false, node.parent)
                    }
                });
        if should_rebuild {
            self.rebuild_visible();
        } else if let Some(parent) = parent
            && parent != self.tree.root_id
        {
            self.selected = Some(parent);
            self.ensure_selection_visible();
        }
    }

    fn toggle_selected(&mut self) {
        let Some(node_id) = self.selected else {
            return;
        };
        let expanded = self
            .tree
            .nodes
            .get(node_id)
            .is_some_and(|node| node.kind == NodeKind::Directory && node.expanded);
        if expanded {
            self.collapse_selected();
        } else {
            self.expand_selected();
        }
    }

    fn toggle_mark_selected(&mut self) {
        let Some(node_id) = self.selected else {
            return;
        };
        let Some(path) = self.tree.nodes.get(node_id).map(|node| node.path.clone()) else {
            return;
        };
        if self.marked.remove(&path) {
            self.notice = Some("Item unmarked".to_owned());
            return;
        }
        if self.has_marked_ancestor(&path) || self.has_marked_tree_ancestor(node_id) {
            self.notice = Some("Item is already included by a marked parent".to_owned());
            return;
        }
        let descendants: Vec<_> = self
            .marked
            .iter()
            .filter(|marked| {
                (marked.starts_with(&path) && marked.as_path() != path)
                    || self
                        .tree
                        .nodes
                        .iter()
                        .find(|(_, node)| node.path.as_path() == marked.as_path())
                        .is_some_and(|(candidate, _)| self.is_tree_descendant(candidate, node_id))
            })
            .cloned()
            .collect();
        for descendant in descendants {
            self.marked.remove(&descendant);
        }
        self.marked.insert(path);
        self.notice = Some("Item marked".to_owned());
    }

    fn has_marked_ancestor(&self, path: &Path) -> bool {
        let mut parent = path.parent();
        while let Some(parent_path) = parent {
            if self.marked.contains(parent_path) {
                return true;
            }
            if parent_path == self.root {
                break;
            }
            parent = parent_path.parent();
        }
        false
    }

    fn has_marked_tree_ancestor(&self, node_id: NodeId) -> bool {
        let mut parent = self.tree.nodes.get(node_id).and_then(|node| node.parent);
        while let Some(parent_id) = parent {
            if self
                .tree
                .nodes
                .get(parent_id)
                .is_some_and(|node| self.marked.contains(&node.path))
            {
                return true;
            }
            parent = self.tree.nodes.get(parent_id).and_then(|node| node.parent);
        }
        false
    }

    fn is_tree_descendant(&self, candidate: NodeId, ancestor: NodeId) -> bool {
        let mut parent = self.tree.nodes.get(candidate).and_then(|node| node.parent);
        while let Some(parent_id) = parent {
            if parent_id == ancestor {
                return true;
            }
            parent = self.tree.nodes.get(parent_id).and_then(|node| node.parent);
        }
        false
    }

    fn confirm_marked_delete(&mut self) {
        let mut items: Vec<_> = self
            .marked
            .iter()
            .map(|path| DeleteItem {
                node_id: self
                    .tree
                    .nodes
                    .iter()
                    .find(|(_, node)| node.path == *path)
                    .map(|(node_id, _)| node_id),
                path: path.clone(),
            })
            .collect();
        if items.is_empty() {
            self.notice = Some("No items marked".to_owned());
            return;
        }
        items.sort_by(|left, right| left.path.cmp(&right.path));
        self.mode = AppMode::ConfirmDelete { items, multi: true };
    }

    fn start_delete(&mut self, items: Vec<DeleteItem>, multi: bool) {
        for item in &items {
            if let Err(error) = validate_delete_target(&self.root, &item.path) {
                self.show_error(error.to_string());
                return;
            }
        }
        if items.is_empty() {
            self.mode = AppMode::Browse;
            return;
        }

        let request = DeleteRequest {
            generation: self.generation,
            root: self.root.clone(),
            items: items.clone(),
        };
        match self.file_worker.send(request) {
            Ok(()) => {
                let item_count = items.len();
                self.mode = AppMode::Deleting { items, multi };
                self.notice = Some(format!("Moving {item_count} item(s) to Trash…"));
            }
            Err(error) => self.show_error(error.to_string()),
        }
    }

    fn handle_delete_result(&mut self, result: crate::delete::DeleteResult) {
        if result.generation != self.generation {
            return;
        }
        let (items, multi) = match &self.mode {
            AppMode::Deleting { items, multi } => (items.clone(), *multi),
            _ => return,
        };
        let belongs_to_request = |path: &Path| items.iter().any(|item| item.path == path);
        if !result
            .moved
            .iter()
            .all(|item| belongs_to_request(&item.path))
            || !result
                .failures
                .iter()
                .all(|failure| belongs_to_request(&failure.path))
        {
            self.show_error("Trash worker returned an unexpected item".to_owned());
            return;
        }

        let moved_count = result.moved.len();
        let failure_count = result.failures.len();
        let known_sizes: Vec<_> = result
            .moved
            .iter()
            .filter_map(|item| {
                item.node_id
                    .and_then(|node_id| self.tree.nodes.get(node_id))
                    .and_then(|node| node.usage)
                    .or_else(|| {
                        self.analysis_index.as_ref().and_then(|index| {
                            index
                                .files
                                .iter()
                                .find(|file| file.path == item.path)
                                .map(|file| file.usage)
                                .or_else(|| {
                                    index
                                        .directories
                                        .iter()
                                        .find(|directory| directory.path == item.path)
                                        .map(|directory| directory.usage)
                                })
                        })
                    })
                    .map(|usage| usage.size(self.size_mode))
            })
            .collect();
        let known_total = known_sizes.iter().copied().fold(0, u64::saturating_add);
        let freed = if known_sizes.len() == moved_count {
            format!("{} freed", format_size(known_total))
        } else if known_sizes.is_empty() {
            "size unknown".to_owned()
        } else {
            format!("at least {} freed", format_size(known_total))
        };

        let notice = if failure_count == 0 && !multi && moved_count == 1 {
            let path = &result.moved[0].path;
            let name = path
                .file_name()
                .unwrap_or(path.as_os_str())
                .to_string_lossy();
            format!("Moved \"{name}\" to Trash — {freed}")
        } else if failure_count == 0 {
            format!("Moved {moved_count} items to Trash — {freed}")
        } else {
            let failure = &result.failures[0];
            format!(
                "Moved {moved_count} items; {failure_count} failed — {}: {}",
                failure.path.display(),
                failure.message
            )
        };

        if self.quit_after_delete {
            self.notice = Some(notice);
            self.should_quit = true;
        } else if moved_count > 0 {
            if let Err(error) = self.apply_deleted_nodes(&result.moved, notice) {
                self.show_error(error.to_string());
            }
        } else if failure_count > 0 {
            self.show_error(notice);
        }
        self.quit_after_delete = false;
    }

    fn apply_deleted_nodes(&mut self, moved: &[DeleteItem], notice: String) -> Result<()> {
        let previous_index = self.selected_index().unwrap_or(0);
        let previous_selection = self.selected;
        let mut ancestors = HashSet::new();

        for item in moved {
            self.cache.invalidate_subtree(&item.path);
            let node_id = item.node_id.or_else(|| {
                self.tree
                    .nodes
                    .iter()
                    .find(|(_, node)| node.path == item.path)
                    .map(|(node_id, _)| node_id)
            });
            let mut parent = node_id
                .and_then(|node_id| self.tree.nodes.get(node_id))
                .and_then(|node| node.parent);
            while let Some(parent_id) = parent {
                if parent_id == self.tree.root_id {
                    break;
                }
                ancestors.insert(parent_id);
                parent = self.tree.nodes.get(parent_id).and_then(|node| node.parent);
            }
        }

        for item in moved {
            let node_id = item.node_id.or_else(|| {
                self.tree
                    .nodes
                    .iter()
                    .find(|(_, node)| node.path == item.path)
                    .map(|(node_id, _)| node_id)
            });
            if let Some(node_id) = node_id {
                self.tree.remove_subtree(node_id);
            }
            self.marked.retain(|path| !path.starts_with(&item.path));
            if let Some(index) = &mut self.analysis_index {
                let removed_usage = index
                    .directories
                    .iter()
                    .find(|directory| directory.path == item.path)
                    .map(|directory| directory.usage)
                    .or_else(|| {
                        index
                            .files
                            .iter()
                            .find(|file| file.path == item.path)
                            .map(|file| file.usage)
                    })
                    .unwrap_or_default();
                for directory in &mut index.directories {
                    if item.path.starts_with(&directory.path) && directory.path != item.path {
                        directory.usage = subtract_usage(directory.usage, removed_usage);
                    }
                }
                index.remove_subtree(&item.path);
            }
        }

        let selection_was_removed =
            previous_selection.is_some_and(|selected| !self.tree.nodes.contains_key(selected));
        if selection_was_removed {
            self.selected = None;
        }
        self.mode = AppMode::Browse;
        self.notice = Some(notice);
        self.sort_dirty = true;

        let mut ancestors: Vec<_> = ancestors
            .into_iter()
            .filter(|node_id| self.tree.nodes.contains_key(*node_id))
            .collect();
        ancestors.sort_by(|left, right| {
            self.tree.nodes[*left]
                .path
                .cmp(&self.tree.nodes[*right].path)
        });
        for ancestor in ancestors {
            self.rescan_size(ancestor)?;
        }

        self.rebuild_visible();
        self.rebuild_analysis_rows();
        if selection_was_removed && !self.visible.is_empty() {
            self.select_index(previous_index.min(self.visible.len() - 1));
        }
        Ok(())
    }

    fn show_error(&mut self, message: String) {
        self.notice = Some(message.clone());
        self.mode = AppMode::ErrorDialog(message);
    }
}

fn normalize_query(value: &str) -> Option<String> {
    let trimmed = value.trim();
    (!trimmed.is_empty()).then(|| trimmed.to_owned())
}

fn append_group_rows(
    rows: &mut Vec<AnalysisRow>,
    section: &str,
    groups: std::collections::BTreeMap<String, GroupStats>,
) {
    for (name, group) in groups {
        rows.push(AnalysisRow {
            label: name,
            path: None,
            usage: group.usage,
            files: group.files,
            detail: section.to_owned(),
            indent: 0,
        });
    }
}

fn compare_analysis_rows(
    left: &AnalysisRow,
    right: &AnalysisRow,
    sort: SortSpec,
    size_mode: SizeMode,
) -> std::cmp::Ordering {
    let ordering = match sort.key {
        SortKey::Size => left.usage.size(size_mode).cmp(&right.usage.size(size_mode)),
        SortKey::Files => left.files.cmp(&right.files),
        SortKey::Name => left.label.to_lowercase().cmp(&right.label.to_lowercase()),
        SortKey::Kind => left.detail.cmp(&right.detail),
    };
    let ordering = match sort.direction {
        SortDirection::Ascending => ordering,
        SortDirection::Descending => ordering.reverse(),
    };
    ordering.then_with(|| left.path.cmp(&right.path))
}

fn compare_file_records(
    left: &FileRecord,
    right: &FileRecord,
    sort: SortSpec,
    size_mode: SizeMode,
) -> std::cmp::Ordering {
    let ordering = match sort.key {
        SortKey::Size => left.usage.size(size_mode).cmp(&right.usage.size(size_mode)),
        SortKey::Files => std::cmp::Ordering::Equal,
        SortKey::Name => left.path.file_name().cmp(&right.path.file_name()),
        SortKey::Kind => left.category.cmp(&right.category),
    };
    let ordering = match sort.direction {
        SortDirection::Ascending => ordering,
        SortDirection::Descending => ordering.reverse(),
    };
    ordering.then_with(|| left.path.cmp(&right.path))
}

fn replace_usage(
    total: crate::tree::UsageStats,
    previous: crate::tree::UsageStats,
    current: crate::tree::UsageStats,
) -> crate::tree::UsageStats {
    crate::tree::UsageStats {
        logical: total
            .logical
            .saturating_sub(previous.logical)
            .saturating_add(current.logical),
        physical: total
            .physical
            .saturating_sub(previous.physical)
            .saturating_add(current.physical),
        files: total
            .files
            .saturating_sub(previous.files)
            .saturating_add(current.files),
    }
}

fn subtract_usage(
    total: crate::tree::UsageStats,
    removed: crate::tree::UsageStats,
) -> crate::tree::UsageStats {
    crate::tree::UsageStats {
        logical: total.logical.saturating_sub(removed.logical),
        physical: total.physical.saturating_sub(removed.physical),
        files: total.files.saturating_sub(removed.files),
    }
}

struct MouseCapture {
    enabled: bool,
}

impl MouseCapture {
    fn new(enabled: bool) -> io::Result<Self> {
        let mut capture = Self { enabled: false };
        capture.set_enabled(enabled)?;
        Ok(capture)
    }

    fn set_enabled(&mut self, enabled: bool) -> io::Result<()> {
        if self.enabled == enabled {
            return Ok(());
        }
        if enabled {
            execute!(io::stdout(), EnableMouseCapture)?;
        } else {
            execute!(io::stdout(), DisableMouseCapture)?;
        }
        self.enabled = enabled;
        Ok(())
    }
}

impl Drop for MouseCapture {
    fn drop(&mut self) {
        if self.enabled {
            let _ = execute!(io::stdout(), DisableMouseCapture);
        }
    }
}

#[cfg(test)]
mod tests {
    use std::{ffi::OsString, fs};

    use tempfile::tempdir;

    use super::*;
    use crate::{
        analytics::{AnalysisEvent, AnalysisIndex, DirectoryRecord, FileCategory, FileRecord},
        filter::FilterExpression,
        scanner::{DiscoveredEntry, ScanOutcome, ScanWarnings},
        tree::{FileIdentity, NodeKind, UsageStats},
        worker::WorkerEvent,
    };

    fn entry(name: &str, size: u64) -> DiscoveredEntry {
        entry_with_kind(name, NodeKind::File, Some(size))
    }

    fn entry_with_kind(name: &str, kind: NodeKind, size: Option<u64>) -> DiscoveredEntry {
        DiscoveredEntry {
            path: PathBuf::from("/tmp").join(name),
            name: OsString::from(name),
            kind,
            usage: size.map(|size| UsageStats {
                logical: size,
                physical: size * 2,
                files: u64::from(kind == NodeKind::File),
            }),
            error: None,
            identity: Some(FileIdentity {
                device: 1,
                inode: size.unwrap_or_default(),
                modified_seconds: 1,
                modified_nanoseconds: 0,
            }),
            modified: None,
            mountpoint: false,
        }
    }

    fn analysis_file(root: &Path, name: &str, size: u64) -> FileRecord {
        let path = root.join(name);
        FileRecord {
            path,
            usage: UsageStats {
                logical: size,
                physical: size / 2,
                files: 1,
            },
            identity: FileIdentity {
                device: 1,
                inode: size,
                modified_seconds: 1,
                modified_nanoseconds: 0,
            },
            modified_seconds: 1,
            modified_nanoseconds: 0,
            extension: Some("bin".to_owned()),
            category: FileCategory::Other,
        }
    }

    #[test]
    fn selection_survives_reordering_and_size_mode_changes() {
        let root_dir = tempdir().unwrap();
        let root = fs::canonicalize(root_dir.path()).unwrap();
        let mut app = App::new(root.clone(), 1).unwrap();
        app.tree = Tree::new(root);
        let small = app
            .tree
            .add_child(app.tree.root_id, entry("small", 1))
            .unwrap();
        app.tree.add_child(app.tree.root_id, entry("large", 10));
        app.rebuild_visible();
        app.selected = Some(small);

        app.size_mode = SizeMode::Physical;
        app.tree.sort_all_loaded(app.sort, app.size_mode);
        app.rebuild_visible();

        assert_eq!(app.selected, Some(small));
        assert_eq!(app.selected_index(), Some(1));
    }

    #[test]
    fn stale_generation_result_is_ignored() {
        let root_dir = tempdir().unwrap();
        let root = fs::canonicalize(root_dir.path()).unwrap();
        let mut app = App::new(root, 1).unwrap();
        let pending = app.pending_jobs;
        let root_id = app.tree.root_id;

        app.handle_worker_event(WorkerEvent::ChildrenLoadFailed {
            generation: app.generation + 1,
            node_id: root_id,
            message: "stale".to_owned(),
        });

        assert_eq!(app.pending_jobs, pending);
        assert_ne!(app.notice.as_deref(), Some("stale"));
    }

    #[test]
    fn vim_navigation_and_scrolling_work() {
        let root_dir = tempdir().unwrap();
        let root = fs::canonicalize(root_dir.path()).unwrap();
        let mut app = App::new(root.clone(), 1).unwrap();
        app.tree = Tree::new(root);
        for index in 0..10 {
            app.tree
                .add_child(app.tree.root_id, entry(&format!("file-{index}"), index));
        }
        app.rebuild_visible();
        app.set_page_size(3);
        app.handle_browse_key(KeyEvent::new(KeyCode::Char('G'), KeyModifiers::SHIFT));
        assert_eq!(app.selected_index(), Some(9));
        assert_eq!(app.scroll_offset, 7);
        app.handle_browse_key(KeyEvent::new(KeyCode::Char('k'), KeyModifiers::NONE));
        assert_eq!(app.selected_index(), Some(8));
        app.handle_browse_key(KeyEvent::new(KeyCode::Char('g'), KeyModifiers::NONE));
        assert_eq!(app.selected_index(), Some(0));
    }

    #[test]
    fn filter_keeps_ancestor_context_and_search_selects_match() {
        let root_dir = tempdir().unwrap();
        let root = fs::canonicalize(root_dir.path()).unwrap();
        let mut app = App::new(root.clone(), 1).unwrap();
        app.tree = Tree::new(root);
        let parent = app
            .tree
            .add_child(
                app.tree.root_id,
                entry_with_kind("parent", NodeKind::Directory, Some(10)),
            )
            .unwrap();
        let needle = app.tree.add_child(parent, entry("needle", 2)).unwrap();
        app.filter_query = Some("needle".to_owned());
        app.rebuild_visible();
        assert_eq!(app.visible.len(), 2);

        app.search_query = Some("needle".to_owned());
        app.find_search_match(1, false);
        assert_eq!(app.selected, Some(needle));
    }

    #[test]
    fn cancelling_scan_advances_generation_and_resets_pending_nodes() {
        let root_dir = tempdir().unwrap();
        let root = fs::canonicalize(root_dir.path()).unwrap();
        let mut app = App::new(root, 1).unwrap();
        let generation = app.generation;
        app.cancel_scan();
        assert_eq!(app.generation, generation + 1);
        assert_eq!(app.pending_jobs, 0);
        assert!(!app.tree.nodes[app.tree.root_id].children_loading);
    }

    #[test]
    fn parent_navigation_replaces_root_and_remembers_previous_root() {
        let workspace = tempdir().unwrap();
        let child = workspace.path().join("child");
        fs::create_dir(&child).unwrap();
        let root = fs::canonicalize(&child).unwrap();
        let expected_parent = fs::canonicalize(workspace.path()).unwrap();
        let mut app = App::new(root.clone(), 1).unwrap();

        app.navigate_parent();

        assert_eq!(app.root, expected_parent);
        assert_eq!(app.pending_select_path, Some(root));
    }

    #[test]
    fn mouse_click_selects_and_marker_column_marks() {
        let root_dir = tempdir().unwrap();
        let root = fs::canonicalize(root_dir.path()).unwrap();
        let mut app = App::new(root.clone(), 1).unwrap();
        app.tree = Tree::new(root);
        let node = app
            .tree
            .add_child(app.tree.root_id, entry("file", 1))
            .unwrap();
        app.rebuild_visible();
        app.set_list_area(Rect::new(5, 4, 80, 10));
        app.handle_mouse(MouseEvent {
            kind: MouseEventKind::Down(MouseButton::Left),
            column: 6,
            row: 4,
            modifiers: KeyModifiers::NONE,
        });
        assert_eq!(app.selected, Some(node));
        assert!(app.is_marked(node));
    }

    #[test]
    fn space_marks_multiple_items_and_x_opens_batch_confirmation() {
        let root_dir = tempdir().unwrap();
        let root = fs::canonicalize(root_dir.path()).unwrap();
        let mut app = App::new(root.clone(), 1).unwrap();
        app.tree = Tree::new(root);
        let first = app
            .tree
            .add_child(app.tree.root_id, entry("first", 1))
            .unwrap();
        let second = app
            .tree
            .add_child(app.tree.root_id, entry("second", 2))
            .unwrap();
        app.rebuild_visible();

        app.selected = Some(first);
        app.toggle_mark_selected();
        app.selected = Some(second);
        app.toggle_mark_selected();
        app.confirm_marked_delete();

        assert!(app.is_marked(first));
        assert!(app.is_marked(second));
        assert!(matches!(
            &app.mode,
            AppMode::ConfirmDelete {
                items,
                multi: true
            } if items.len() == 2
        ));
    }

    #[test]
    fn marking_parent_removes_redundant_child_mark() {
        let root_dir = tempdir().unwrap();
        let root = fs::canonicalize(root_dir.path()).unwrap();
        let mut app = App::new(root.clone(), 1).unwrap();
        app.tree = Tree::new(root);
        let parent = app
            .tree
            .add_child(
                app.tree.root_id,
                entry_with_kind("parent", NodeKind::Directory, Some(10)),
            )
            .unwrap();
        let child = app.tree.add_child(parent, entry("child", 5)).unwrap();
        app.tree.nodes[parent].expanded = true;
        app.rebuild_visible();

        app.selected = Some(child);
        app.toggle_mark_selected();
        app.selected = Some(parent);
        app.toggle_mark_selected();

        assert!(app.is_marked(parent));
        assert!(!app.is_marked(child));
        assert_eq!(app.marked.len(), 1);
    }

    #[test]
    fn delete_removes_only_subtree_and_rescans_ancestors() {
        let root_dir = tempdir().unwrap();
        let root = fs::canonicalize(root_dir.path()).unwrap();
        let mut app = App::new(root.clone(), 1).unwrap();
        app.tree = Tree::new(root);
        let root_id = app.tree.root_id;
        let grandparent = app
            .tree
            .add_child(
                root_id,
                entry_with_kind("grandparent", NodeKind::Directory, Some(20)),
            )
            .unwrap();
        let parent = app
            .tree
            .add_child(
                grandparent,
                entry_with_kind("parent", NodeKind::Directory, Some(10)),
            )
            .unwrap();
        let child = app.tree.add_child(parent, entry("child", 5)).unwrap();
        let sibling = app.tree.add_child(root_id, entry("sibling", 2)).unwrap();
        app.tree.nodes[grandparent].expanded = true;
        app.tree.nodes[parent].expanded = true;
        app.rebuild_visible();
        app.selected = Some(child);
        app.marked.insert(app.tree.nodes[child].path.clone());

        app.apply_deleted_nodes(
            &[DeleteItem {
                node_id: Some(child),
                path: PathBuf::from("/tmp/child"),
            }],
            "deleted".to_owned(),
        )
        .unwrap();

        assert_eq!(app.tree.root_id, root_id);
        assert!(app.tree.nodes.contains_key(grandparent));
        assert!(app.tree.nodes.contains_key(parent));
        assert!(app.tree.nodes.contains_key(sibling));
        assert!(!app.tree.nodes.contains_key(child));
        assert_eq!(app.tree.nodes[grandparent].scan_state, ScanState::Queued);
        assert_eq!(app.tree.nodes[parent].scan_state, ScanState::Queued);
        assert_eq!(app.selected, Some(sibling));
    }

    #[test]
    fn stale_size_revision_cannot_overwrite_rescan() {
        let root_dir = tempdir().unwrap();
        let root = fs::canonicalize(root_dir.path()).unwrap();
        let mut app = App::new(root.clone(), 1).unwrap();
        app.tree = Tree::new(root);
        let directory = app
            .tree
            .add_child(
                app.tree.root_id,
                entry_with_kind("directory", NodeKind::Directory, None),
            )
            .unwrap();
        app.tree.nodes[directory].scan_revision = 2;
        app.tree.nodes[directory].scan_state = ScanState::Queued;

        app.handle_worker_event(WorkerEvent::SizeCalculated {
            generation: app.generation,
            node_id: directory,
            scan_revision: 1,
            outcome: ScanOutcome {
                usage: UsageStats {
                    logical: 999,
                    physical: 1024,
                    files: 1,
                },
                warnings: ScanWarnings::default(),
                fatal_error: None,
                cancelled: false,
                mounts_skipped: 0,
            },
        });

        assert_eq!(app.tree.nodes[directory].usage, None);
        assert_eq!(app.tree.nodes[directory].scan_state, ScanState::Queued);
    }

    #[test]
    fn percentage_uses_current_size_mode_and_parent() {
        let root_dir = tempdir().unwrap();
        let root = fs::canonicalize(root_dir.path()).unwrap();
        let mut app = App::new(root.clone(), 1).unwrap();
        app.tree = Tree::new(root);
        let first = app
            .tree
            .add_child(app.tree.root_id, entry("first", 25))
            .unwrap();
        app.tree.add_child(app.tree.root_id, entry("second", 75));
        app.rebuild_visible();

        assert_eq!(app.percentage(first), Some(25.0));
        app.size_mode = SizeMode::Physical;
        assert_eq!(app.percentage(first), Some(25.0));
    }

    #[test]
    fn phase_three_tabs_filter_and_path_marking_work_together() {
        let root_dir = tempdir().unwrap();
        let root = fs::canonicalize(root_dir.path()).unwrap();
        let mut app = App::new(root.clone(), 1).unwrap();
        let mut index = AnalysisIndex::new(root.clone(), true, 1);
        index.files = vec![
            analysis_file(&root, "small.bin", 10),
            analysis_file(&root, "large.bin", 10 * 1024 * 1024),
        ];
        index.complete = true;
        index.completed_at = Some(2);
        app.analysis_index = Some(index);
        app.analysis_status = AnalysisStatus::Ready;

        app.handle_browse_key(KeyEvent::new(KeyCode::Tab, KeyModifiers::NONE));
        assert_eq!(app.active_view, ViewKind::Largest);
        assert_eq!(app.analysis_rows.len(), 2);
        assert!(app.analysis_rows[0].label.contains("large"));

        app.filter_query = Some("size>1MiB".to_owned());
        app.filter_expression = FilterExpression::parse("size>1MiB").unwrap();
        app.rebuild_analysis_rows();
        assert_eq!(app.analysis_rows.len(), 1);

        app.toggle_mark_current();
        let marked = app.analysis_rows[0].path.clone().unwrap();
        assert!(app.is_path_marked(&marked));
        app.confirm_current_delete();
        assert!(matches!(
            &app.mode,
            AppMode::ConfirmDelete { items, multi: false }
                if items.len() == 1 && items[0].node_id.is_none() && items[0].path == marked
        ));
    }

    #[test]
    fn number_keys_select_all_phase_three_views() {
        let root_dir = tempdir().unwrap();
        let root = fs::canonicalize(root_dir.path()).unwrap();
        let mut app = App::new(root.clone(), 1).unwrap();
        let mut index = AnalysisIndex::new(root, true, 1);
        index.complete = true;
        app.analysis_index = Some(index);
        app.analysis_status = AnalysisStatus::Ready;

        for (key, expected) in [
            ('1', ViewKind::Tree),
            ('2', ViewKind::Largest),
            ('3', ViewKind::Types),
            ('4', ViewKind::Duplicates),
            ('5', ViewKind::Changes),
        ] {
            app.handle_browse_key(KeyEvent::new(KeyCode::Char(key), KeyModifiers::NONE));
            assert_eq!(app.active_view, expected);
        }
    }

    #[test]
    fn watcher_refreshes_only_the_affected_loaded_subtree() {
        let root_dir = tempdir().unwrap();
        let root = fs::canonicalize(root_dir.path()).unwrap();
        let dirty_path = root.join("dirty");
        fs::create_dir(&dirty_path).unwrap();
        fs::write(dirty_path.join("changed"), b"new").unwrap();
        let sibling_path = root.join("sibling");
        fs::write(&sibling_path, b"keep").unwrap();
        let mut app = App::new(root.clone(), 1).unwrap();
        app.tree = Tree::new(root.clone());
        let dirty = app
            .tree
            .add_child(
                app.tree.root_id,
                entry_with_kind("dirty", NodeKind::Directory, Some(1)),
            )
            .unwrap();
        app.tree.nodes[dirty].path = dirty_path.clone();
        app.tree.nodes[dirty].children_loaded = true;
        let old_child = app.tree.add_child(dirty, entry("old", 1)).unwrap();
        app.tree.nodes[old_child].path = dirty_path.join("old");
        let sibling = app
            .tree
            .add_child(app.tree.root_id, entry("sibling", 4))
            .unwrap();
        app.tree.nodes[sibling].path = sibling_path;

        app.handle_watch_batch(crate::watcher::WatchBatch {
            paths: vec![dirty_path.join("changed")],
            full_rescan: false,
            warning: None,
        });

        assert!(app.tree.nodes.contains_key(dirty));
        assert!(app.tree.nodes.contains_key(sibling));
        assert!(!app.tree.nodes.contains_key(old_child));
        assert_eq!(app.tree.nodes[dirty].scan_state, ScanState::Queued);
    }

    #[test]
    fn watcher_analysis_refresh_merges_only_the_changed_slice() {
        let root_dir = tempdir().unwrap();
        let root = fs::canonicalize(root_dir.path()).unwrap();
        let dirty = root.join("dirty");
        fs::create_dir(&dirty).unwrap();
        fs::write(dirty.join("new.bin"), vec![0_u8; 20]).unwrap();
        let mut app = App::new(root.clone(), 1).unwrap();
        let mut index = AnalysisIndex::new(root.clone(), true, 1);
        index.complete = true;
        index.files = vec![
            analysis_file(&dirty, "old.bin", 10),
            analysis_file(&root, "sibling.bin", 4),
        ];
        index.directories = vec![
            DirectoryRecord {
                path: root.clone(),
                usage: UsageStats {
                    logical: 14,
                    physical: 7,
                    files: 2,
                },
                identity: index.files[0].identity,
                modified_seconds: 1,
                modified_nanoseconds: 0,
            },
            DirectoryRecord {
                path: dirty.clone(),
                usage: UsageStats {
                    logical: 10,
                    physical: 5,
                    files: 1,
                },
                identity: index.files[0].identity,
                modified_seconds: 1,
                modified_nanoseconds: 0,
            },
        ];
        app.analysis_index = Some(index);
        app.analysis_status = AnalysisStatus::Ready;

        app.refresh_analysis_subtrees(vec![dirty.clone()]);
        assert!(
            app.analysis_index
                .as_ref()
                .unwrap()
                .files
                .iter()
                .any(|file| file.path.ends_with("sibling.bin"))
        );
        assert!(app.analysis_refresh_pending.contains(&dirty));

        let current = analysis_file(&dirty, "new.bin", 20);
        app.handle_analysis_event(AnalysisEvent::FileBatch {
            generation: app.generation,
            files: vec![current.clone()],
        });
        app.handle_analysis_event(AnalysisEvent::Finished {
            generation: app.generation,
            scan_root: dirty.clone(),
            completed_at: 2,
            directories: vec![DirectoryRecord {
                path: dirty,
                usage: current.usage,
                identity: current.identity,
                modified_seconds: 1,
                modified_nanoseconds: 0,
            }],
            issues: Vec::new(),
            mounts_skipped: 0,
        });

        let index = app.analysis_index.as_ref().unwrap();
        assert!(index.complete);
        assert_eq!(index.files.len(), 2);
        assert_eq!(index.root_usage().logical, 24);
    }

    #[test]
    fn largest_view_does_not_rebuild_and_sort_for_every_file_batch() {
        let root_dir = tempdir().unwrap();
        let root = fs::canonicalize(root_dir.path()).unwrap();
        let mut app = App::new(root.clone(), 1).unwrap();
        app.active_view = ViewKind::Largest;
        app.analysis_status = AnalysisStatus::Indexing;
        app.analysis_index = Some(AnalysisIndex::new(root.clone(), true, 1));

        for batch in 0..32 {
            let files = (0..256)
                .map(|index| analysis_file(&root, &format!("file-{batch:02}-{index:03}.bin"), 1))
                .collect();
            app.handle_analysis_event(AnalysisEvent::FileBatch {
                generation: app.generation,
                files,
            });
        }

        assert_eq!(app.analysis_index.as_ref().unwrap().files.len(), 8_192);
        assert_eq!(app.analysis_known_usage().logical, 8_192);
        assert!(app.analysis_rows.is_empty());
    }

    #[test]
    fn largest_view_caps_materialized_rows_for_very_large_indexes() {
        let root_dir = tempdir().unwrap();
        let root = fs::canonicalize(root_dir.path()).unwrap();
        let mut app = App::new(root.clone(), 1).unwrap();
        app.active_view = ViewKind::Largest;
        app.analysis_status = AnalysisStatus::Ready;
        let mut index = AnalysisIndex::new(root.clone(), true, 1);
        index.complete = true;
        index.files = (0..=MAX_ANALYSIS_ROWS)
            .map(|number| analysis_file(&root, &format!("file-{number}.bin"), number as u64))
            .collect();
        app.analysis_index = Some(index);

        app.rebuild_analysis_rows();

        assert_eq!(app.analysis_rows.len(), MAX_ANALYSIS_ROWS);
        assert!(app.analysis_rows_truncated);
        assert_eq!(app.analysis_rows[0].usage.logical, MAX_ANALYSIS_ROWS as u64);
    }
}
