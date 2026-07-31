//! Root-child ("column") compatibility layer over the tree root.

use super::ContainerData;
use super::ContainerTree;
use super::DetachedNode;
use super::LayoutElement;
use super::NodeData;
use super::NodeKey;
use crate::layout::tile::Tile;

impl<W: LayoutElement> ContainerTree<W> {
    /// Number of root-level children (columns).
    pub(in crate::layout) fn root_children_len(&self) -> usize {
        let root_key = match self.root {
            Some(key) => key,
            None => return 0,
        };

        match self.get_node(root_key) {
            Some(NodeData::Leaf(_)) => 1,
            Some(NodeData::Container(container)) => container.children.len(),
            None => 0,
        }
    }

    pub(in crate::layout) fn root_container(&self) -> Option<&ContainerData> {
        let root_key = self.root?;
        self.get_container(root_key)
    }

    /// Index of currently focused root child, if any.
    pub(in crate::layout) fn focused_root_index(&self) -> Option<usize> {
        let root_key = self.root?;
        if let Some(key) = self.focused_key {
            if key == root_key {
                return Some(0);
            }
            let mut child = key;
            let mut parent = self.parent_of(child)?;
            while parent != root_key {
                child = parent;
                parent = self.parent_of(child)?;
            }
            return self.child_index(root_key, child);
        }

        match self.get_node(root_key) {
            Some(NodeData::Leaf(_)) => Some(0),
            Some(NodeData::Container(container)) => {
                let focus_path = self.focus_path();
                if focus_path.is_empty() {
                    container.focused_child_index()
                } else {
                    Some(focus_path[0])
                }
            }
            None => None,
        }
    }

    /// Focus root child at index, descending to the first leaf.
    pub(in crate::layout) fn focus_root_child(&mut self, idx: usize) -> bool {
        self.clear_focus_history();
        let root_key = match self.root {
            Some(key) => key,
            None => return false,
        };

        match self.get_node(root_key) {
            Some(NodeData::Leaf(_)) => {
                if idx == 0 {
                    self.focus_node_key(root_key);
                    true
                } else {
                    false
                }
            }
            Some(NodeData::Container(container)) => {
                if idx >= container.children.len() {
                    return false;
                }
                let child_key = container.child_key(idx);
                if let Some(child_key) = child_key {
                    self.focus_node_key(child_key);
                    true
                } else {
                    false
                }
            }
            None => false,
        }
    }

    /// Move a root child from one index to another
    pub(in crate::layout) fn move_root_child(&mut self, from: usize, to: usize) -> bool {
        self.clear_focus_history();
        let root_key = match self.root {
            Some(key) => key,
            None => return false,
        };

        let container = match self.get_container_mut(root_key) {
            Some(c) => c,
            None => return false,
        };

        if from >= container.children.len() || to >= container.children.len() {
            return false;
        }

        let node_key = container.children.remove(from);
        let percent = container.child_percents.remove(from);
        container.children.insert(to, node_key);
        container.child_percents.insert(to, percent);
        container.normalize_child_percents();

        self.resync_focus();
        true
    }

    /// Insert a detached subtree at root level.
    pub(in crate::layout) fn insert_subtree_at_root(
        &mut self,
        index: usize,
        subtree: DetachedNode<W>,
        focus: bool,
    ) {
        let node_key = self.insert_subtree(subtree);
        self.insert_key_at_root(index, node_key, focus);
    }

    /// Focus nth (1-based) leaf within the given root child.
    pub(in crate::layout) fn focus_leaf_in_root_child(
        &mut self,
        child_idx: usize,
        leaf_idx: usize,
    ) -> bool {
        self.clear_focus_history();
        if leaf_idx == 0 {
            return false;
        }
        let mut paths = self.leaf_paths_under(&[child_idx]);
        if paths.is_empty() {
            return false;
        }
        if leaf_idx > paths.len() {
            return false;
        }
        let path = paths.remove(leaf_idx - 1);
        if let Some(key) = self.get_node_key_at_path(&path) {
            self.focus_node_key(key);
            true
        } else {
            false
        }
    }

    /// Focus the first leaf in the currently focused root child.
    pub(in crate::layout) fn focus_top_in_current_column(&mut self) -> bool {
        let idx = match self.focused_root_index() {
            Some(idx) => idx,
            None => return false,
        };
        self.focus_leaf_in_root_child(idx, 1)
    }

    /// Focus the last leaf in the currently focused root child.
    pub(in crate::layout) fn focus_bottom_in_current_column(&mut self) -> bool {
        let idx = match self.focused_root_index() {
            Some(idx) => idx,
            None => return false,
        };
        let paths = self.leaf_paths_under(&[idx]);
        if let Some(path) = paths.last() {
            if let Some(key) = self.get_node_key_at_path(path) {
                self.focus_node_key(key);
                return true;
            }
            false
        } else {
            false
        }
    }

    pub(in crate::layout) fn append_leaf(&mut self, tile: Tile<W>, focus: bool) {
        self.insert_leaf_at(self.root_children_len(), tile, focus);
    }

    pub(in crate::layout) fn insert_leaf_at(&mut self, index: usize, tile: Tile<W>, focus: bool) {
        let tile_key = self.insert_node(NodeData::Leaf(tile));
        self.insert_key_at_root(index, tile_key, focus);
    }

    pub(super) fn insert_key_at_root(&mut self, index: usize, node_key: NodeKey, focus: bool) {
        let insert_idx = {
            let container_key = self.ensure_root_container();
            let container = self.get_container(container_key).unwrap();
            let idx = index.min(container.children.len());

            if let Some(container) = self.get_container_mut(container_key) {
                container.insert_child(idx, node_key);

                idx
            } else {
                idx
            }
        };
        let container_key = self.ensure_root_container();
        if let Some(container) = self.get_container(container_key) {
            if container.child_key(insert_idx) == Some(node_key) {
                self.set_parent(node_key, Some(container_key));
            }
        }

        self.settle_focus_after_insert(node_key, focus);
    }
}
