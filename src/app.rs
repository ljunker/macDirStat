use std::{
    collections::HashSet,
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
    cache::ScanCache,
    config::Settings,
    delete::{DeleteItem, DeleteRequest, FileOperationWorker, validate_delete_target},
    format::format_size,
    scanner::{ScanOptions, root_device},
    theme::ThemeKind,
    tree::{
        Node, NodeId, NodeKind, ScanState, SizeMode, SortDirection, SortKey, SortSpec, Tree,
        VisibleNode,
    },
    ui,
    worker::{ScanJob, WorkerEvent, WorkerPool},
};

const EVENT_POLL: Duration = Duration::from_millis(33);
const SORT_INTERVAL: Duration = Duration::from_millis(200);
const CACHE_FLUSH_INTERVAL: Duration = Duration::from_secs(2);
const DOUBLE_CLICK_INTERVAL: Duration = Duration::from_millis(400);

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum PromptKind {
    Search,
    Filter,
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
        node_ids: Vec<NodeId>,
        multi: bool,
    },
    Deleting {
        node_ids: Vec<NodeId>,
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
    marked: HashSet<NodeId>,
    generation: u64,
    root_device: u64,
    one_file_system: bool,
    scan_pool: WorkerPool,
    file_worker: FileOperationWorker,
    cache: ScanCache,
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
            marked: HashSet::new(),
            generation,
            root_device,
            one_file_system: settings.one_file_system,
            scan_pool: WorkerPool::new(settings.workers, generation)?,
            file_worker: FileOperationWorker::new()?,
            cache,
            sort_dirty: false,
            last_sort: Instant::now(),
            last_cache_flush: Instant::now(),
            should_quit: false,
            quit_after_delete: false,
            pending_select_path: None,
            last_click: None,
        };
        app.queue_children(root_id)?;
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
        let progress = if self.pending_jobs == 0 {
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
        if let Some(notice) = &self.notice {
            parts.push(notice.clone());
        }
        parts.join(" | ")
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
        self.ensure_selection_visible();
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
        self.visible = self
            .tree
            .flatten_visible_filtered(self.filter_query.as_deref());
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
            AppMode::Help => self.handle_help_key(key),
            AppMode::Input { .. } => self.handle_prompt_key(key),
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
            KeyCode::Up | KeyCode::Char('k') => self.move_selection(-1),
            KeyCode::Down | KeyCode::Char('j') => self.move_selection(1),
            KeyCode::Home | KeyCode::Char('g') => self.select_index(0),
            KeyCode::End | KeyCode::Char('G') => {
                self.select_index(self.visible.len().saturating_sub(1));
            }
            KeyCode::PageUp => self.move_selection(-(self.page_size as isize)),
            KeyCode::PageDown => self.move_selection(self.page_size as isize),
            KeyCode::Right | KeyCode::Char('l') => self.expand_selected(),
            KeyCode::Left | KeyCode::Char('h') => self.collapse_selected(),
            KeyCode::Enter => self.toggle_selected(),
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
                self.rebuild_visible();
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
                        PromptKind::Filter => {
                            self.filter_query = normalize_query(value);
                            filter_changed = true;
                        }
                    }
                    close = true;
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
            filter_changed = true;
        }
        if close {
            self.mode = AppMode::Browse;
        }
        if filter_changed {
            self.rebuild_visible();
        }
        if commit_search {
            self.find_search_match(1, false);
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
        self.notice = Some(format!(
            "Sorted by {} {}",
            self.sort.key.label(),
            self.sort.direction.symbol()
        ));
    }

    fn cancel_scan(&mut self) {
        if self.pending_jobs == 0 {
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
        self.notice = Some(format!("Cancelled {cancelled} scan job(s)"));
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
        self.tree = Tree::new(root);
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
        self.cache_hits = 0;
        self.mounts_skipped = 0;
        self.filter_query = None;
        self.search_query = None;
        self.pending_select_path = pending_select_path;
        self.queue_children(self.tree.root_id)
    }

    fn advance_generation(&mut self) -> Result<()> {
        self.generation = self
            .generation
            .checked_add(1)
            .ok_or_else(|| anyhow!("Scan generation overflow"))?;
        self.scan_pool.set_generation(self.generation);
        Ok(())
    }

    fn apply_pending_selection(&mut self) {
        let Some(path) = self.pending_select_path.take() else {
            return;
        };
        if let Some((node_id, _)) = self.tree.nodes.iter().find(|(_, node)| node.path == path) {
            self.selected = Some(node_id);
            self.ensure_selection_visible();
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
            .filter_map(|item| {
                self.tree
                    .nodes
                    .get(item.node_id)
                    .and_then(|node| node.usage)
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

    fn show_error(&mut self, message: String) {
        self.notice = Some(message.clone());
        self.mode = AppMode::ErrorDialog(message);
    }
}

fn normalize_query(value: &str) -> Option<String> {
    let trimmed = value.trim();
    (!trimmed.is_empty()).then(|| trimmed.to_owned())
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
}
