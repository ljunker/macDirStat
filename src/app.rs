use std::{
    collections::HashSet,
    io,
    path::PathBuf,
    time::{Duration, Instant},
};

use anyhow::{Result, anyhow};
use crossterm::event::{self, Event, KeyCode, KeyEvent, KeyEventKind, KeyModifiers};
use ratatui::DefaultTerminal;

use crate::{
    delete::{DeleteItem, DeleteRequest, FileOperationWorker, validate_delete_target},
    format::format_size,
    tree::{NodeId, NodeKind, ScanState, Tree, VisibleNode},
    ui,
    worker::{ScanJob, WorkerEvent, WorkerPool},
};

const EVENT_POLL: Duration = Duration::from_millis(33);
const SORT_INTERVAL: Duration = Duration::from_millis(200);

#[derive(Debug)]
pub enum AppMode {
    Browse,
    ConfirmDelete { node_ids: Vec<NodeId>, multi: bool },
    Deleting { node_ids: Vec<NodeId>, multi: bool },
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
    marked: HashSet<NodeId>,
    generation: u64,
    scan_pool: WorkerPool,
    file_worker: FileOperationWorker,
    sort_dirty: bool,
    last_sort: Instant,
    should_quit: bool,
    quit_after_delete: bool,
}

impl App {
    pub fn new(root: PathBuf, workers: usize) -> Result<Self> {
        let generation = 1;
        let tree = Tree::new(root.clone());
        let root_id = tree.root_id;
        let mut app = Self {
            root,
            tree,
            visible: Vec::new(),
            selected: None,
            scroll_offset: 0,
            page_size: 1,
            mode: AppMode::Browse,
            workers,
            pending_jobs: 0,
            errors: 0,
            notice: None,
            marked: HashSet::new(),
            generation,
            scan_pool: WorkerPool::new(workers, generation)?,
            file_worker: FileOperationWorker::new()?,
            sort_dirty: false,
            last_sort: Instant::now(),
            should_quit: false,
            quit_after_delete: false,
        };
        app.queue_children(root_id)?;
        Ok(app)
    }

    pub fn run(&mut self, terminal: &mut DefaultTerminal) -> io::Result<()> {
        while !self.should_quit {
            self.drain_events();
            self.maybe_sort();
            terminal.draw(|frame| ui::render(frame, self))?;

            if event::poll(EVENT_POLL)? {
                match event::read()? {
                    Event::Key(key)
                        if matches!(key.kind, KeyEventKind::Press | KeyEventKind::Repeat) =>
                    {
                        self.handle_key(key);
                    }
                    Event::Resize(_, _) => {}
                    _ => {}
                }
            }
        }
        Ok(())
    }

    pub fn status_text(&self) -> String {
        let size = format_size(self.tree.root_known_size());
        let progress = if self.pending_jobs == 0 {
            format!("Scan complete | {size} | {} errors", self.errors)
        } else {
            format!(
                "Scanning: {} jobs | {size} found | {} workers | {} errors",
                self.pending_jobs, self.workers, self.errors
            )
        };
        let progress = if self.marked.is_empty() {
            progress
        } else {
            format!("{progress} | {} marked", self.marked.len())
        };
        self.notice
            .as_ref()
            .map_or(progress.clone(), |notice| format!("{progress} | {notice}"))
    }

    pub fn is_marked(&self, node_id: NodeId) -> bool {
        self.marked.contains(&node_id)
    }

    pub fn selected_index(&self) -> Option<usize> {
        let selected = self.selected?;
        self.visible
            .iter()
            .position(|visible| visible.node_id == selected)
    }

    pub fn set_page_size(&mut self, page_size: usize) {
        self.page_size = page_size.max(1);
        self.ensure_selection_visible();
    }

    fn queue_children(&mut self, node_id: NodeId) -> Result<()> {
        let Some(node) = self.tree.nodes.get_mut(node_id) else {
            return Ok(());
        };
        if node.children_loaded || node.children_loading || node.kind != NodeKind::Directory {
            return Ok(());
        }
        node.children_loading = true;
        let path = node.path.clone();
        if let Err(error) = self.scan_pool.send(ScanJob::LoadChildren {
            generation: self.generation,
            node_id,
            path,
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
        let Some(node) = self.tree.nodes.get_mut(node_id) else {
            return Ok(());
        };
        if node.kind != NodeKind::Directory
            || (!force && !matches!(node.scan_state, ScanState::NotScanned | ScanState::Error))
        {
            return Ok(());
        }
        node.scan_revision = node
            .scan_revision
            .checked_add(1)
            .ok_or_else(|| anyhow!("Scan revision overflow for {}", node.path.display()))?;
        node.scan_state = ScanState::Queued;
        node.size = None;
        node.error = None;
        node.warning_count = 0;
        let scan_revision = node.scan_revision;
        let path = node.path.clone();
        if let Err(error) = self.scan_pool.send(ScanJob::CalculateSize {
            generation: self.generation,
            node_id,
            scan_revision,
            path,
        }) {
            node.scan_state = ScanState::Error;
            return Err(error);
        }
        self.pending_jobs += 1;
        Ok(())
    }

    fn drain_events(&mut self) {
        while let Ok(event) = self.scan_pool.try_recv() {
            self.handle_worker_event(event);
        }

        while let Ok(result) = self.file_worker.try_recv() {
            self.handle_delete_result(result);
        }
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

                let warning_count = outcome.warnings.error_count;
                let first_warning = outcome.warnings.first_message.clone();
                self.record_warnings(warning_count, first_warning.clone());

                if let Some(node) = self.tree.nodes.get_mut(node_id) {
                    node.children_loading = false;
                    node.children_loaded = true;
                    node.warning_count = node.warning_count.saturating_add(warning_count);
                    if node.error.is_none() {
                        node.error = first_warning;
                    }
                }

                let mut directories = Vec::new();
                for entry in outcome.entries {
                    if let Some(child) = self.tree.add_child(node_id, entry)
                        && self.tree.nodes[child].kind == NodeKind::Directory
                    {
                        directories.push(child);
                    }
                }
                self.tree.sort_children(node_id);
                for directory in directories {
                    if let Err(error) = self.queue_size(directory) {
                        self.show_error(error.to_string());
                    }
                }
                self.rebuild_visible();
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
                let warning_count = outcome.warnings.error_count;
                let first_warning = outcome.warnings.first_message.clone();
                self.record_warnings(warning_count, first_warning.clone());
                if let Some(node) = self.tree.nodes.get_mut(node_id) {
                    node.warning_count = warning_count;
                    node.error = outcome.fatal_error.clone().or(first_warning);
                    if outcome.fatal_error.is_some() {
                        node.size = None;
                        node.scan_state = ScanState::Error;
                    } else {
                        node.size = Some(outcome.size);
                        node.scan_state = ScanState::Complete;
                    }
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
        self.tree.sort_all_loaded();
        self.sort_dirty = false;
        self.last_sort = Instant::now();
        self.rebuild_visible();
    }

    fn rebuild_visible(&mut self) {
        let previous = self.selected;
        self.visible = self.tree.flatten_visible();
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
        if let Some(visible) = self
            .visible
            .get(index.min(self.visible.len().saturating_sub(1)))
        {
            self.selected = Some(visible.node_id);
            self.ensure_selection_visible();
        }
    }

    fn move_selection(&mut self, delta: isize) {
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
            AppMode::ConfirmDelete { node_ids, multi } => match key.code {
                KeyCode::Char('y') | KeyCode::Char('Y') => {
                    let node_ids = node_ids.clone();
                    let multi = *multi;
                    self.start_delete(node_ids, multi);
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
            KeyCode::Up => self.move_selection(-1),
            KeyCode::Down => self.move_selection(1),
            KeyCode::Home | KeyCode::Char('g') => self.select_index(0),
            KeyCode::End => self.select_index(self.visible.len().saturating_sub(1)),
            KeyCode::PageUp => self.move_selection(-(self.page_size as isize)),
            KeyCode::PageDown => self.move_selection(self.page_size as isize),
            KeyCode::Right => self.expand_selected(),
            KeyCode::Left => self.collapse_selected(),
            KeyCode::Enter => self.toggle_selected(),
            KeyCode::Char('d') => {
                if let Some(node_id) = self.selected {
                    self.mode = AppMode::ConfirmDelete {
                        node_ids: vec![node_id],
                        multi: false,
                    };
                }
            }
            KeyCode::Char(' ') => self.toggle_mark_selected(),
            KeyCode::Char('x') => self.confirm_marked_delete(),
            KeyCode::Char('r') => {
                if let Err(error) = self.reload_root(Some("Refreshing current root".to_owned())) {
                    self.show_error(error.to_string());
                }
            }
            _ => {}
        }
    }

    fn expand_selected(&mut self) {
        let Some(node_id) = self.selected else {
            return;
        };
        let should_load = if let Some(node) = self.tree.nodes.get_mut(node_id) {
            if node.kind != NodeKind::Directory {
                return;
            }
            node.expanded = true;
            !node.children_loaded && !node.children_loading
        } else {
            false
        };
        if should_load && let Err(error) = self.queue_children(node_id) {
            self.show_error(error.to_string());
        }
        self.rebuild_visible();
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
        if self.marked.remove(&node_id) {
            self.notice = Some("Item unmarked".to_owned());
            return;
        }

        if self.has_marked_ancestor(node_id) {
            self.notice = Some("Item is already included by a marked parent".to_owned());
            return;
        }

        let descendants: Vec<_> = self
            .marked
            .iter()
            .copied()
            .filter(|marked| self.is_descendant_of(*marked, node_id))
            .collect();
        for descendant in descendants {
            self.marked.remove(&descendant);
        }
        self.marked.insert(node_id);
        self.notice = Some("Item marked".to_owned());
    }

    fn has_marked_ancestor(&self, node_id: NodeId) -> bool {
        let mut parent = self.tree.nodes.get(node_id).and_then(|node| node.parent);
        while let Some(parent_id) = parent {
            if self.marked.contains(&parent_id) {
                return true;
            }
            parent = self.tree.nodes.get(parent_id).and_then(|node| node.parent);
        }
        false
    }

    fn is_descendant_of(&self, candidate: NodeId, ancestor: NodeId) -> bool {
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
        let mut node_ids: Vec<_> = self
            .marked
            .iter()
            .copied()
            .filter(|node_id| self.tree.nodes.contains_key(*node_id))
            .collect();
        if node_ids.is_empty() {
            self.notice = Some("No items marked".to_owned());
            return;
        }
        node_ids.sort_by(|left, right| {
            self.tree.nodes[*left]
                .path
                .cmp(&self.tree.nodes[*right].path)
        });
        self.mode = AppMode::ConfirmDelete {
            node_ids,
            multi: true,
        };
    }

    fn start_delete(&mut self, node_ids: Vec<NodeId>, multi: bool) {
        let mut items = Vec::with_capacity(node_ids.len());
        for &node_id in &node_ids {
            let Some(node) = self.tree.nodes.get(node_id) else {
                self.show_error("A selected item no longer exists".to_owned());
                return;
            };
            if let Err(error) = validate_delete_target(&self.root, &node.path) {
                self.show_error(error.to_string());
                return;
            }
            items.push(DeleteItem {
                node_id,
                path: node.path.clone(),
            });
        }
        if items.is_empty() {
            self.mode = AppMode::Browse;
            return;
        }

        let request = DeleteRequest {
            generation: self.generation,
            root: self.root.clone(),
            items,
        };
        match self.file_worker.send(request) {
            Ok(()) => {
                let item_count = node_ids.len();
                self.mode = AppMode::Deleting { node_ids, multi };
                self.notice = Some(format!("Moving {item_count} item(s) to Trash…"));
            }
            Err(error) => self.show_error(error.to_string()),
        }
    }

    fn handle_delete_result(&mut self, result: crate::delete::DeleteResult) {
        if result.generation != self.generation {
            return;
        }
        let (node_ids, multi) = match &self.mode {
            AppMode::Deleting { node_ids, multi } => (node_ids.clone(), *multi),
            _ => return,
        };
        let belongs_to_request = |node_id: NodeId| node_ids.contains(&node_id);
        if !result
            .moved
            .iter()
            .all(|item| belongs_to_request(item.node_id))
            || !result
                .failures
                .iter()
                .all(|failure| belongs_to_request(failure.node_id))
        {
            self.show_error("Trash worker returned an unexpected item".to_owned());
            return;
        }

        let moved_count = result.moved.len();
        let failure_count = result.failures.len();
        let known_sizes: Vec<_> = result
            .moved
            .iter()
            .filter_map(|item| self.tree.nodes.get(item.node_id).and_then(|node| node.size))
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
            let mut parent = self
                .tree
                .nodes
                .get(item.node_id)
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
            for removed in self.tree.remove_subtree(item.node_id) {
                self.marked.remove(&removed);
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
        if selection_was_removed && !self.visible.is_empty() {
            self.select_index(previous_index.min(self.visible.len() - 1));
        }
        Ok(())
    }

    fn reload_root(&mut self, notice: Option<String>) -> Result<()> {
        self.generation = self
            .generation
            .checked_add(1)
            .ok_or_else(|| anyhow!("Scan generation overflow"))?;
        self.scan_pool.set_generation(self.generation);
        self.tree = Tree::new(self.root.clone());
        self.visible.clear();
        self.selected = None;
        self.scroll_offset = 0;
        self.pending_jobs = 0;
        self.errors = 0;
        self.notice = notice;
        self.marked.clear();
        self.mode = AppMode::Browse;
        self.sort_dirty = false;
        self.last_sort = Instant::now();
        self.queue_children(self.tree.root_id)
    }

    fn show_error(&mut self, message: String) {
        self.notice = Some(message.clone());
        self.mode = AppMode::ErrorDialog(message);
    }
}

#[cfg(test)]
mod tests {
    use std::{ffi::OsString, fs};

    use tempfile::tempdir;

    use super::*;
    use crate::{
        scanner::{DiscoveredEntry, ScanOutcome, ScanWarnings},
        tree::NodeKind,
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
            size,
            error: None,
        }
    }

    #[test]
    fn selection_survives_reordering() {
        let root_dir = tempdir().unwrap();
        let root = fs::canonicalize(root_dir.path()).unwrap();
        let mut app = App::new(root.clone(), 1).unwrap();
        app.tree = Tree::new(root);
        let small = app
            .tree
            .add_child(app.tree.root_id, entry("small", 1))
            .unwrap();
        app.tree
            .add_child(app.tree.root_id, entry("large", 10))
            .unwrap();
        app.rebuild_visible();
        app.selected = Some(small);

        app.tree.sort_all_loaded();
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
    fn scrolling_keeps_selection_on_screen() {
        let root_dir = tempdir().unwrap();
        let root = fs::canonicalize(root_dir.path()).unwrap();
        let mut app = App::new(root.clone(), 1).unwrap();
        app.tree = Tree::new(root);
        for index in 0..10 {
            app.tree
                .add_child(app.tree.root_id, entry(&format!("file-{index}"), index))
                .unwrap();
        }
        app.rebuild_visible();
        app.set_page_size(3);
        app.select_index(9);

        assert_eq!(app.scroll_offset, 7);
    }

    #[test]
    fn g_jumps_to_the_first_visible_entry() {
        let root_dir = tempdir().unwrap();
        let root = fs::canonicalize(root_dir.path()).unwrap();
        let mut app = App::new(root.clone(), 1).unwrap();
        app.tree = Tree::new(root);
        for index in 0..5 {
            app.tree
                .add_child(app.tree.root_id, entry(&format!("file-{index}"), index))
                .unwrap();
        }
        app.rebuild_visible();
        app.set_page_size(2);
        app.select_index(4);

        app.handle_browse_key(KeyEvent::new(KeyCode::Char('g'), KeyModifiers::NONE));

        assert_eq!(app.selected_index(), Some(0));
        assert_eq!(app.scroll_offset, 0);
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
        app.handle_browse_key(KeyEvent::new(KeyCode::Char(' '), KeyModifiers::NONE));
        app.selected = Some(second);
        app.handle_browse_key(KeyEvent::new(KeyCode::Char(' '), KeyModifiers::NONE));
        app.handle_browse_key(KeyEvent::new(KeyCode::Char('x'), KeyModifiers::NONE));

        assert!(app.is_marked(first));
        assert!(app.is_marked(second));
        assert!(matches!(
            &app.mode,
            AppMode::ConfirmDelete {
                node_ids,
                multi: true
            } if node_ids.len() == 2
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
        app.marked.insert(child);

        app.apply_deleted_nodes(
            &[DeleteItem {
                node_id: child,
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
        assert!(!app.is_marked(child));
        assert_eq!(app.tree.nodes[grandparent].scan_revision, 1);
        assert_eq!(app.tree.nodes[grandparent].scan_state, ScanState::Queued);
        assert_eq!(app.tree.nodes[parent].scan_revision, 1);
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
                size: 999,
                warnings: ScanWarnings::default(),
                fatal_error: None,
                cancelled: false,
            },
        });

        assert_eq!(app.tree.nodes[directory].size, None);
        assert_eq!(app.tree.nodes[directory].scan_state, ScanState::Queued);
    }
}
