//! Translation between tree paths, node keys and windows.

use super::ContainerTree;
use super::LayoutElement;
use super::NodeData;
use super::NodeKey;

impl<W: LayoutElement> ContainerTree<W> {
    /// Resolve a tree path to a node key.
    ///
    /// Paths address the tree positionally and are invalidated by any structural change, so
    /// they only survive at the edges (IPC, drag-and-drop hit results, tests). This is the
    /// single place where such a path re-enters the key-based world.
    pub(in crate::layout) fn node_at_path(&self, path: &[usize]) -> Option<NodeKey> {
        self.node_key_for_path_or_root(path)
    }

    /// Helper: get node key at path
    pub(super) fn get_node_key_at_path(&self, path: &[usize]) -> Option<NodeKey> {
        let mut current_key = self.root;

        for &idx in path {
            match self.get_node(current_key)? {
                NodeData::Container(container) => {
                    current_key = container.child_key(idx)?;
                }
                NodeData::Leaf(_) => return None,
            }
        }

        Some(current_key)
    }

    pub(super) fn sync_container_focus_from_key(&mut self, key: NodeKey) {
        self.raise_in_focus_order(key);
    }

    pub(super) fn leaf_under_key(&self, mut key: NodeKey) -> Option<NodeKey> {
        loop {
            match self.get_node(key)? {
                NodeData::Leaf(_) => return Some(key),
                NodeData::Container(container) => {
                    if container.children.is_empty() {
                        return None;
                    }
                    key = self.active_child(key)?;
                }
            }
        }
    }

    pub(super) fn first_leaf_key(&self) -> Option<NodeKey> {
        let root_key = self.root;
        self.leaf_under_key(root_key)
    }

    pub(super) fn focus_node_key(&mut self, key: NodeKey) {
        let Some(leaf_key) = self.leaf_under_key(key) else {
            self.focused_key = None;
            self.selected_key = None;
            return;
        };
        self.focused_key = Some(leaf_key);
        self.selected_key = None;
        self.sync_container_focus_from_key(leaf_key);
    }

    /// Move logical/seat focus without changing the active branch of any switcher container.
    ///
    /// This deliberately is not a general focus operation. Sway uses it for one move outcome:
    /// the moved view keeps seat focus while the sibling tab/stack selected during tree surgery
    /// remains visible.
    pub(super) fn set_seat_focus_preserving_switcher(&mut self, leaf_key: NodeKey) {
        if matches!(self.get_node(leaf_key), Some(NodeData::Leaf(_))) {
            self.focused_key = Some(leaf_key);
        }
    }

    /// Find a node by key and return path to it.
    pub(super) fn find_node_path(&self, target_key: NodeKey) -> Option<Vec<usize>> {
        let root_key = self.root;
        if target_key == root_key {
            return Some(Vec::new());
        }

        let mut path_rev = Vec::new();
        let mut current = target_key;
        while current != root_key {
            let parent = self.parent_of(current)?;
            let idx = self.child_index(parent, current)?;
            path_rev.push(idx);
            current = parent;
        }

        path_rev.reverse();
        Some(path_rev)
    }

    pub(super) fn clear_focus_history(&mut self) {
        // Focus history is tracked per-container via focus_stack.
    }

    /// Find a window by ID and return its node key.
    ///
    /// Prefer this over [`Self::find_window`] plus a path lookup: it avoids materializing a
    /// path only to resolve it back into the key the caller actually wanted.
    pub(in crate::layout) fn window_key(&self, window_id: &W::Id) -> Option<NodeKey> {
        self.nodes.iter().find_map(|(key, node)| match node {
            NodeData::Leaf(tile) if tile.window().id() == window_id => Some(key),
            _ => None,
        })
    }

    /// Find a window by ID and return path to it
    pub(in crate::layout) fn find_window(&self, window_id: &W::Id) -> Option<Vec<usize>> {
        let root_key = self.root;
        let mut path = Vec::new();
        self.find_window_in_node(root_key, window_id, &mut path)
    }

    /// Helper: recursively find window in node
    pub(super) fn find_window_in_node(
        &self,
        node_key: NodeKey,
        window_id: &W::Id,
        path: &mut Vec<usize>,
    ) -> Option<Vec<usize>> {
        match self.get_node(node_key)? {
            NodeData::Leaf(tile) => {
                if tile.window().id() == window_id {
                    Some(path.clone())
                } else {
                    None
                }
            }
            NodeData::Container(container) => {
                for (idx, &child_key) in container.children.iter().enumerate() {
                    path.push(idx);
                    if let Some(result) = self.find_window_in_node(child_key, window_id, path) {
                        return Some(result);
                    }
                    path.pop();
                }
                None
            }
        }
    }

    pub(super) fn node_key_for_path_or_root(&self, path: &[usize]) -> Option<NodeKey> {
        self.get_node_key_at_path(path)
    }
}
