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
        let root_key = self.root;

        match self.get_node(root_key) {
            Some(NodeData::Leaf(_)) => 1,
            Some(NodeData::Container(container)) => container.children.len(),
            None => 0,
        }
    }

    pub(in crate::layout) fn root_container(&self) -> Option<&ContainerData> {
        let root_key = self.root;
        self.get_container(root_key)
    }

    /// Index of currently focused root child, if any.
    pub(in crate::layout) fn focused_root_index(&self) -> Option<usize> {
        let root_key = self.root;
        if let Some(key) = self.focused_key() {
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
            Some(NodeData::Container(_)) => {
                // Nothing focused yet: fall back to the container's own focus history.
                let Some(focused_key) = self.effective_focused_key() else {
                    return self.active_child_index(root_key);
                };
                // Walk up from the focused leaf to the root child that holds it.
                let mut child = focused_key;
                while let Some(parent) = self.parent_of(child) {
                    if parent == root_key {
                        return self.child_index(root_key, child);
                    }
                    child = parent;
                }
                self.active_child_index(root_key)
            }
            None => None,
        }
    }

    /// Focus root child at index, descending to the first leaf.
    pub(in crate::layout) fn focus_root_child(&mut self, idx: usize) -> bool {
        self.clear_focus_history();
        let root_key = self.root;

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
        let root_key = self.root;

        let container = match self.get_container_mut(root_key) {
            Some(c) => c,
            None => return false,
        };

        if from >= container.children.len() || to >= container.children.len() {
            return false;
        }

        let node_key = container.children.remove(from);
        container.children.insert(to, node_key);
        container.fractions.move_child(from, to);

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
        let Some(child_key) = self
            .get_container(self.root)
            .and_then(|root| root.child_key(child_idx))
        else {
            return false;
        };
        let leaves = self.leaf_keys_under(child_key);
        let Some(&key) = leaves.get(leaf_idx - 1) else {
            return false;
        };
        self.focus_node_key(key);
        true
    }

    /// Focus the first leaf under the focused root child, whatever its layout.
    pub(in crate::layout) fn focus_first_leaf_in_focused_root_child(&mut self) -> bool {
        let idx = match self.focused_root_index() {
            Some(idx) => idx,
            None => return false,
        };
        self.focus_leaf_in_root_child(idx, 1)
    }

    /// Focus the last leaf under the focused root child, whatever its layout.
    pub(in crate::layout) fn focus_last_leaf_in_focused_root_child(&mut self) -> bool {
        let idx = match self.focused_root_index() {
            Some(idx) => idx,
            None => return false,
        };
        let Some(child_key) = self
            .get_container(self.root)
            .and_then(|root| root.child_key(idx))
        else {
            return false;
        };
        let Some(&key) = self.leaf_keys_under(child_key).last() else {
            return false;
        };
        self.focus_node_key(key);
        true
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
            let container_key = self.root;
            let container = self.get_container(container_key).unwrap();
            let idx = index.min(container.children.len());

            if let Some(container) = self.get_container_mut(container_key) {
                container.insert_child(idx, node_key);

                idx
            } else {
                idx
            }
        };
        let container_key = self.root;
        if let Some(container) = self.get_container(container_key) {
            if container.child_key(insert_idx) == Some(node_key) {
                self.set_parent(node_key, Some(container_key));
            }
        }

        self.settle_focus_after_insert(node_key, focus);
    }
}
