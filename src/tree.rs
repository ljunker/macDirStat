use std::{
    cmp::Ordering,
    ffi::{OsStr, OsString},
    path::PathBuf,
};

use slotmap::{SlotMap, new_key_type};

use crate::scanner::DiscoveredEntry;

new_key_type! {
    pub struct NodeId;
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum NodeKind {
    File,
    Directory,
    Symlink,
    Other,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ScanState {
    NotScanned,
    Queued,
    Scanning,
    Complete,
    Error,
}

#[derive(Debug)]
pub struct Node {
    pub path: PathBuf,
    pub name: OsString,
    pub kind: NodeKind,
    pub size: Option<u64>,
    pub scan_state: ScanState,
    pub scan_revision: u64,
    pub expanded: bool,
    pub children_loaded: bool,
    pub children_loading: bool,
    pub children: Vec<NodeId>,
    pub parent: Option<NodeId>,
    pub error: Option<String>,
    pub warning_count: usize,
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
            size: None,
            scan_state: ScanState::NotScanned,
            scan_revision: 0,
            expanded: true,
            children_loaded: false,
            children_loading: false,
            children: Vec::new(),
            parent: None,
            error: None,
            warning_count: 0,
        }
    }

    fn from_entry(entry: DiscoveredEntry, parent: NodeId) -> Self {
        let has_error = entry.error.is_some();
        let scan_state = if has_error {
            ScanState::Error
        } else if entry.kind == NodeKind::Directory {
            ScanState::NotScanned
        } else {
            ScanState::Complete
        };

        Self {
            path: entry.path,
            name: entry.name,
            kind: entry.kind,
            size: entry.size,
            scan_state,
            scan_revision: 0,
            expanded: false,
            children_loaded: false,
            children_loading: false,
            children: Vec::new(),
            parent: Some(parent),
            error: entry.error,
            warning_count: usize::from(has_error),
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct VisibleNode {
    pub node_id: NodeId,
    pub depth: usize,
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

    pub fn flatten_visible(&self) -> Vec<VisibleNode> {
        let mut visible = Vec::new();
        if let Some(root) = self.nodes.get(self.root_id) {
            for &child in &root.children {
                self.flatten_from(child, 0, &mut visible);
            }
        }
        visible
    }

    fn flatten_from(&self, node_id: NodeId, depth: usize, output: &mut Vec<VisibleNode>) {
        let Some(node) = self.nodes.get(node_id) else {
            return;
        };
        output.push(VisibleNode { node_id, depth });
        if node.kind == NodeKind::Directory && node.expanded {
            for &child in &node.children {
                self.flatten_from(child, depth + 1, output);
            }
        }
    }

    pub fn sort_children(&mut self, parent: NodeId) {
        let Some(parent_node) = self.nodes.get(parent) else {
            return;
        };
        let mut children = parent_node.children.clone();
        children.sort_by(|left, right| compare_nodes(&self.nodes[*left], &self.nodes[*right]));
        if let Some(parent_node) = self.nodes.get_mut(parent) {
            parent_node.children = children;
        }
    }

    pub fn sort_all_loaded(&mut self) {
        let parents: Vec<_> = self
            .nodes
            .iter()
            .filter_map(|(id, node)| (!node.children.is_empty()).then_some(id))
            .collect();
        for parent in parents {
            self.sort_children(parent);
        }
    }

    pub fn root_known_size(&self) -> u64 {
        self.nodes
            .get(self.root_id)
            .into_iter()
            .flat_map(|root| &root.children)
            .filter_map(|id| self.nodes.get(*id).and_then(|node| node.size))
            .fold(0, u64::saturating_add)
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

fn compare_nodes(left: &Node, right: &Node) -> Ordering {
    match (sort_category(left), sort_category(right)) {
        (left_category, right_category) if left_category != right_category => {
            left_category.cmp(&right_category)
        }
        (1, 1) => right
            .size
            .cmp(&left.size)
            .then_with(|| compare_names(&left.name, &right.name)),
        _ => compare_names(&left.name, &right.name),
    }
}

fn sort_category(node: &Node) -> u8 {
    if matches!(
        node.scan_state,
        ScanState::NotScanned | ScanState::Queued | ScanState::Scanning
    ) {
        0
    } else if node.size.is_some() {
        1
    } else {
        2
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

    fn entry(name: &str, kind: NodeKind, size: Option<u64>) -> DiscoveredEntry {
        DiscoveredEntry {
            path: PathBuf::from("/tmp").join(name),
            name: OsString::from(name),
            kind,
            size,
            error: None,
        }
    }

    #[test]
    fn flattening_respects_expansion() {
        let mut tree = Tree::new(PathBuf::from("/tmp"));
        let a = tree
            .add_child(tree.root_id, entry("a", NodeKind::Directory, Some(10)))
            .unwrap();
        let b = tree
            .add_child(tree.root_id, entry("b", NodeKind::File, Some(5)))
            .unwrap();
        let x = tree
            .add_child(a, entry("x", NodeKind::File, Some(1)))
            .unwrap();

        assert_eq!(
            tree.flatten_visible(),
            vec![
                VisibleNode {
                    node_id: a,
                    depth: 0
                },
                VisibleNode {
                    node_id: b,
                    depth: 0
                }
            ]
        );

        tree.nodes[a].expanded = true;
        assert_eq!(
            tree.flatten_visible(),
            vec![
                VisibleNode {
                    node_id: a,
                    depth: 0
                },
                VisibleNode {
                    node_id: x,
                    depth: 1
                },
                VisibleNode {
                    node_id: b,
                    depth: 0
                }
            ]
        );
    }

    #[test]
    fn sorting_puts_pending_first_then_descending_sizes() {
        let mut tree = Tree::new(PathBuf::from("/tmp"));
        let small = tree
            .add_child(tree.root_id, entry("small", NodeKind::File, Some(2)))
            .unwrap();
        let pending = tree
            .add_child(tree.root_id, entry("pending", NodeKind::Directory, None))
            .unwrap();
        let large = tree
            .add_child(tree.root_id, entry("large", NodeKind::File, Some(10)))
            .unwrap();

        tree.sort_children(tree.root_id);
        assert_eq!(tree.nodes[tree.root_id].children, [pending, large, small]);
    }

    #[test]
    fn removing_subtree_keeps_parent_and_siblings() {
        let mut tree = Tree::new(PathBuf::from("/tmp"));
        let directory = tree
            .add_child(
                tree.root_id,
                entry("directory", NodeKind::Directory, Some(10)),
            )
            .unwrap();
        let child = tree
            .add_child(directory, entry("child", NodeKind::File, Some(5)))
            .unwrap();
        let sibling = tree
            .add_child(tree.root_id, entry("sibling", NodeKind::File, Some(2)))
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
