use std::{
    cmp::Ordering,
    ffi::{OsStr, OsString},
    path::PathBuf,
    time::SystemTime,
};

use clap::ValueEnum;
use serde::{Deserialize, Serialize};
use slotmap::{SlotMap, new_key_type};

use crate::scanner::DiscoveredEntry;

new_key_type! {
    pub struct NodeId;
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq, Serialize, Deserialize)]
pub struct UsageStats {
    pub logical: u64,
    pub physical: u64,
    pub files: u64,
}

impl UsageStats {
    pub fn size(self, mode: SizeMode) -> u64 {
        match mode {
            SizeMode::Logical => self.logical,
            SizeMode::Physical => self.physical,
        }
    }

    pub fn saturating_add(self, other: Self) -> Self {
        Self {
            logical: self.logical.saturating_add(other.logical),
            physical: self.physical.saturating_add(other.physical),
            files: self.files.saturating_add(other.files),
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct FileIdentity {
    pub device: u64,
    pub inode: u64,
    pub modified_seconds: i64,
    pub modified_nanoseconds: i64,
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq, ValueEnum, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
#[value(rename_all = "kebab-case")]
pub enum SizeMode {
    #[default]
    Logical,
    Physical,
}

impl SizeMode {
    pub fn toggled(self) -> Self {
        match self {
            Self::Logical => Self::Physical,
            Self::Physical => Self::Logical,
        }
    }

    pub fn label(self) -> &'static str {
        match self {
            Self::Logical => "logical",
            Self::Physical => "physical",
        }
    }
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq, ValueEnum, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
#[value(rename_all = "kebab-case")]
pub enum SortKey {
    #[default]
    Size,
    Name,
    Files,
    Kind,
}

impl SortKey {
    pub fn next(self) -> Self {
        match self {
            Self::Size => Self::Name,
            Self::Name => Self::Files,
            Self::Files => Self::Kind,
            Self::Kind => Self::Size,
        }
    }

    pub fn label(self) -> &'static str {
        match self {
            Self::Size => "size",
            Self::Name => "name",
            Self::Files => "files",
            Self::Kind => "kind",
        }
    }

    pub fn default_direction(self) -> SortDirection {
        match self {
            Self::Size | Self::Files => SortDirection::Descending,
            Self::Name | Self::Kind => SortDirection::Ascending,
        }
    }
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq, ValueEnum, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
#[value(rename_all = "kebab-case")]
pub enum SortDirection {
    Ascending,
    #[default]
    Descending,
}

impl SortDirection {
    pub fn reversed(self) -> Self {
        match self {
            Self::Ascending => Self::Descending,
            Self::Descending => Self::Ascending,
        }
    }

    pub fn symbol(self) -> &'static str {
        match self {
            Self::Ascending => "↑",
            Self::Descending => "↓",
        }
    }
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct SortSpec {
    pub key: SortKey,
    pub direction: SortDirection,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum NodeKind {
    File,
    Directory,
    Symlink,
    Other,
}

impl NodeKind {
    pub fn label(self) -> &'static str {
        match self {
            Self::File => "File",
            Self::Directory => "Directory",
            Self::Symlink => "Symlink",
            Self::Other => "Other",
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ScanState {
    NotScanned,
    Queued,
    Scanning,
    Complete,
    Error,
}

impl ScanState {
    pub fn label(self) -> &'static str {
        match self {
            Self::NotScanned => "not scanned",
            Self::Queued => "queued",
            Self::Scanning => "scanning",
            Self::Complete => "complete",
            Self::Error => "error",
        }
    }
}

#[derive(Debug)]
pub struct Node {
    pub path: PathBuf,
    pub name: OsString,
    pub kind: NodeKind,
    pub usage: Option<UsageStats>,
    pub scan_state: ScanState,
    pub scan_revision: u64,
    pub expanded: bool,
    pub children_loaded: bool,
    pub children_loading: bool,
    pub children: Vec<NodeId>,
    pub parent: Option<NodeId>,
    pub error: Option<String>,
    pub warning_count: usize,
    pub identity: Option<FileIdentity>,
    pub modified: Option<SystemTime>,
    pub mountpoint: bool,
    pub cached: bool,
}

impl Node {
    fn root(path: PathBuf) -> Self {
        let name = path
            .file_name()
            .unwrap_or_else(|| path.as_os_str())
            .to_os_string();
        Self {
            path,
            name,
            kind: NodeKind::Directory,
            usage: None,
            scan_state: ScanState::NotScanned,
            scan_revision: 0,
            expanded: true,
            children_loaded: false,
            children_loading: false,
            children: Vec::new(),
            parent: None,
            error: None,
            warning_count: 0,
            identity: None,
            modified: None,
            mountpoint: false,
            cached: false,
        }
    }

    fn from_entry(entry: DiscoveredEntry, parent: NodeId) -> Self {
        let has_error = entry.error.is_some();
        let scan_state = if has_error {
            ScanState::Error
        } else if entry.mountpoint || entry.kind != NodeKind::Directory {
            ScanState::Complete
        } else {
            ScanState::NotScanned
        };

        Self {
            path: entry.path,
            name: entry.name,
            kind: entry.kind,
            usage: entry.usage,
            scan_state,
            scan_revision: 0,
            expanded: false,
            children_loaded: false,
            children_loading: false,
            children: Vec::new(),
            parent: Some(parent),
            error: entry.error,
            warning_count: usize::from(has_error),
            identity: entry.identity,
            modified: entry.modified,
            mountpoint: entry.mountpoint,
            cached: false,
        }
    }

    pub fn matches(&self, query: &str) -> bool {
        let query = query.to_lowercase();
        self.name.to_string_lossy().to_lowercase().contains(&query)
            || self.path.to_string_lossy().to_lowercase().contains(&query)
    }

    pub fn matches_name(&self, query: &str) -> bool {
        self.name
            .to_string_lossy()
            .to_lowercase()
            .contains(&query.to_lowercase())
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct VisibleNode {
    pub node_id: NodeId,
    pub depth: usize,
    pub matched: bool,
}

pub struct Tree {
    pub nodes: SlotMap<NodeId, Node>,
    pub root_id: NodeId,
}

impl Tree {
    pub fn new(root: PathBuf) -> Self {
        let mut nodes = SlotMap::with_key();
        let root_id = nodes.insert(Node::root(root));
        Self { nodes, root_id }
    }

    pub fn add_child(&mut self, parent: NodeId, entry: DiscoveredEntry) -> Option<NodeId> {
        if !self.nodes.contains_key(parent) {
            return None;
        }
        let child = self.nodes.insert(Node::from_entry(entry, parent));
        self.nodes[parent].children.push(child);
        Some(child)
    }

    #[cfg(test)]
    pub fn flatten_visible(&self) -> Vec<VisibleNode> {
        self.flatten_visible_filtered(None)
    }

    pub fn flatten_visible_filtered(&self, filter: Option<&str>) -> Vec<VisibleNode> {
        let filter = filter.filter(|filter| !filter.is_empty());
        let mut visible = Vec::new();
        if let Some(root) = self.nodes.get(self.root_id) {
            for &child in &root.children {
                if let Some(filter) = filter {
                    self.flatten_filtered_from(child, 0, filter, &mut visible);
                } else {
                    self.flatten_from(child, 0, &mut visible);
                }
            }
        }
        visible
    }

    fn flatten_from(&self, node_id: NodeId, depth: usize, output: &mut Vec<VisibleNode>) {
        let Some(node) = self.nodes.get(node_id) else {
            return;
        };
        output.push(VisibleNode {
            node_id,
            depth,
            matched: false,
        });
        if node.kind == NodeKind::Directory && node.expanded {
            for &child in &node.children {
                self.flatten_from(child, depth + 1, output);
            }
        }
    }

    fn flatten_filtered_from(
        &self,
        node_id: NodeId,
        depth: usize,
        filter: &str,
        output: &mut Vec<VisibleNode>,
    ) -> bool {
        let Some(node) = self.nodes.get(node_id) else {
            return false;
        };
        let matched = node.matches_name(filter);
        let mut descendants = Vec::new();
        let mut descendant_matched = false;
        if node.kind == NodeKind::Directory {
            for &child in &node.children {
                descendant_matched |=
                    self.flatten_filtered_from(child, depth + 1, filter, &mut descendants);
            }
        }
        if matched || descendant_matched {
            output.push(VisibleNode {
                node_id,
                depth,
                matched,
            });
            output.extend(descendants);
            true
        } else {
            false
        }
    }

    pub fn all_loaded(&self) -> Vec<NodeId> {
        let mut output = Vec::new();
        if let Some(root) = self.nodes.get(self.root_id) {
            for &child in &root.children {
                self.collect_all(child, &mut output);
            }
        }
        output
    }

    fn collect_all(&self, node_id: NodeId, output: &mut Vec<NodeId>) {
        let Some(node) = self.nodes.get(node_id) else {
            return;
        };
        output.push(node_id);
        for &child in &node.children {
            self.collect_all(child, output);
        }
    }

    pub fn expand_ancestors(&mut self, node_id: NodeId) {
        let mut parent = self.nodes.get(node_id).and_then(|node| node.parent);
        while let Some(parent_id) = parent {
            if let Some(node) = self.nodes.get_mut(parent_id) {
                node.expanded = true;
                parent = node.parent;
            } else {
                break;
            }
        }
    }

    pub fn sort_children(&mut self, parent: NodeId, spec: SortSpec, size_mode: SizeMode) {
        let Some(parent_node) = self.nodes.get(parent) else {
            return;
        };
        let mut children = parent_node.children.clone();
        children.sort_by(|left, right| {
            compare_nodes(&self.nodes[*left], &self.nodes[*right], spec, size_mode)
        });
        if let Some(parent_node) = self.nodes.get_mut(parent) {
            parent_node.children = children;
        }
    }

    pub fn sort_all_loaded(&mut self, spec: SortSpec, size_mode: SizeMode) {
        let parents: Vec<_> = self
            .nodes
            .iter()
            .filter_map(|(id, node)| (!node.children.is_empty()).then_some(id))
            .collect();
        for parent in parents {
            self.sort_children(parent, spec, size_mode);
        }
    }

    pub fn root_known_usage(&self) -> UsageStats {
        self.nodes
            .get(self.root_id)
            .into_iter()
            .flat_map(|root| &root.children)
            .filter_map(|id| self.nodes.get(*id).and_then(|node| node.usage))
            .fold(UsageStats::default(), UsageStats::saturating_add)
    }

    pub fn remove_subtree(&mut self, node_id: NodeId) -> Vec<NodeId> {
        if node_id == self.root_id || !self.nodes.contains_key(node_id) {
            return Vec::new();
        }

        if let Some(parent) = self.nodes[node_id].parent
            && let Some(parent_node) = self.nodes.get_mut(parent)
        {
            parent_node.children.retain(|child| *child != node_id);
        }

        let mut removed = Vec::new();
        self.remove_subtree_nodes(node_id, &mut removed);
        removed
    }

    fn remove_subtree_nodes(&mut self, node_id: NodeId, removed: &mut Vec<NodeId>) {
        let Some(node) = self.nodes.get(node_id) else {
            return;
        };
        let children = node.children.clone();
        for child in children {
            self.remove_subtree_nodes(child, removed);
        }
        if self.nodes.remove(node_id).is_some() {
            removed.push(node_id);
        }
    }
}

fn compare_nodes(left: &Node, right: &Node, spec: SortSpec, size_mode: SizeMode) -> Ordering {
    let ordering = match spec.key {
        SortKey::Size => compare_optional(
            left.usage.map(|usage| usage.size(size_mode)),
            right.usage.map(|usage| usage.size(size_mode)),
            spec.direction,
        ),
        SortKey::Files => compare_optional(
            left.usage.map(|usage| usage.files),
            right.usage.map(|usage| usage.files),
            spec.direction,
        ),
        SortKey::Name => apply_direction(compare_names(&left.name, &right.name), spec.direction),
        SortKey::Kind => apply_direction(
            kind_order(left.kind).cmp(&kind_order(right.kind)),
            spec.direction,
        ),
    };
    let both_unknown = matches!(spec.key, SortKey::Size | SortKey::Files)
        && left.usage.is_none()
        && right.usage.is_none();
    if both_unknown {
        Ordering::Equal
    } else {
        ordering.then_with(|| compare_names(&left.name, &right.name))
    }
}

fn compare_optional(left: Option<u64>, right: Option<u64>, direction: SortDirection) -> Ordering {
    match (left, right) {
        (Some(left), Some(right)) => apply_direction(left.cmp(&right), direction),
        (Some(_), None) => Ordering::Less,
        (None, Some(_)) => Ordering::Greater,
        (None, None) => Ordering::Equal,
    }
}

fn apply_direction(ordering: Ordering, direction: SortDirection) -> Ordering {
    match direction {
        SortDirection::Ascending => ordering,
        SortDirection::Descending => ordering.reverse(),
    }
}

fn kind_order(kind: NodeKind) -> u8 {
    match kind {
        NodeKind::Directory => 0,
        NodeKind::File => 1,
        NodeKind::Symlink => 2,
        NodeKind::Other => 3,
    }
}

fn compare_names(left: &OsStr, right: &OsStr) -> Ordering {
    left.to_string_lossy()
        .to_lowercase()
        .cmp(&right.to_string_lossy().to_lowercase())
        .then_with(|| left.cmp(right))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn entry(name: &str, kind: NodeKind, usage: Option<UsageStats>) -> DiscoveredEntry {
        DiscoveredEntry {
            path: PathBuf::from("/tmp").join(name),
            name: OsString::from(name),
            kind,
            usage,
            error: None,
            identity: None,
            modified: None,
            mountpoint: false,
        }
    }

    fn usage(size: u64, files: u64) -> Option<UsageStats> {
        Some(UsageStats {
            logical: size,
            physical: size * 2,
            files,
        })
    }

    #[test]
    fn flattening_respects_expansion() {
        let mut tree = Tree::new(PathBuf::from("/tmp"));
        let a = tree
            .add_child(tree.root_id, entry("a", NodeKind::Directory, usage(10, 1)))
            .unwrap();
        let b = tree
            .add_child(tree.root_id, entry("b", NodeKind::File, usage(5, 1)))
            .unwrap();
        let x = tree
            .add_child(a, entry("x", NodeKind::File, usage(1, 1)))
            .unwrap();

        assert_eq!(
            tree.flatten_visible(),
            vec![
                VisibleNode {
                    node_id: a,
                    depth: 0,
                    matched: false,
                },
                VisibleNode {
                    node_id: b,
                    depth: 0,
                    matched: false,
                }
            ]
        );

        tree.nodes[a].expanded = true;
        assert_eq!(tree.flatten_visible()[1].node_id, x);
    }

    #[test]
    fn filter_keeps_matching_descendants_and_ancestors() {
        let mut tree = Tree::new(PathBuf::from("/tmp"));
        let parent = tree
            .add_child(
                tree.root_id,
                entry("parent", NodeKind::Directory, usage(10, 1)),
            )
            .unwrap();
        let match_id = tree
            .add_child(parent, entry("needle.txt", NodeKind::File, usage(1, 1)))
            .unwrap();
        tree.add_child(parent, entry("other", NodeKind::File, usage(1, 1)));

        let visible = tree.flatten_visible_filtered(Some("needle"));
        assert_eq!(visible.len(), 2);
        assert_eq!(visible[0].node_id, parent);
        assert!(!visible[0].matched);
        assert_eq!(visible[1].node_id, match_id);
        assert!(visible[1].matched);

        let visible = tree.flatten_visible_filtered(Some("parent"));
        assert_eq!(visible.len(), 1);
        assert_eq!(visible[0].node_id, parent);
        assert!(visible[0].matched);
    }

    #[test]
    fn sorting_supports_size_name_and_file_count() {
        let mut tree = Tree::new(PathBuf::from("/tmp"));
        let small = tree
            .add_child(tree.root_id, entry("z-small", NodeKind::File, usage(2, 1)))
            .unwrap();
        let pending = tree
            .add_child(tree.root_id, entry("pending", NodeKind::Directory, None))
            .unwrap();
        let pending_second = tree
            .add_child(tree.root_id, entry("a-pending", NodeKind::Directory, None))
            .unwrap();
        let large = tree
            .add_child(tree.root_id, entry("a-large", NodeKind::File, usage(10, 4)))
            .unwrap();

        tree.sort_children(tree.root_id, SortSpec::default(), SizeMode::Logical);
        assert_eq!(
            tree.nodes[tree.root_id].children,
            [large, small, pending, pending_second]
        );

        tree.sort_children(
            tree.root_id,
            SortSpec {
                key: SortKey::Name,
                direction: SortDirection::Ascending,
            },
            SizeMode::Logical,
        );
        assert_eq!(
            tree.nodes[tree.root_id].children,
            [large, pending_second, pending, small]
        );
    }

    #[test]
    fn removing_subtree_keeps_parent_and_siblings() {
        let mut tree = Tree::new(PathBuf::from("/tmp"));
        let directory = tree
            .add_child(
                tree.root_id,
                entry("directory", NodeKind::Directory, usage(10, 1)),
            )
            .unwrap();
        let child = tree
            .add_child(directory, entry("child", NodeKind::File, usage(5, 1)))
            .unwrap();
        let sibling = tree
            .add_child(tree.root_id, entry("sibling", NodeKind::File, usage(2, 1)))
            .unwrap();

        let removed = tree.remove_subtree(directory);

        assert!(removed.contains(&directory));
        assert!(removed.contains(&child));
        assert!(!tree.nodes.contains_key(directory));
        assert!(!tree.nodes.contains_key(child));
        assert!(tree.nodes.contains_key(sibling));
        assert_eq!(tree.nodes[tree.root_id].children, [sibling]);
    }
}
