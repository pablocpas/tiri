//! Translation between tree paths, node keys and windows.

use super::ContainerTree;
use super::LayoutElement;
use super::NodeData;
use super::NodeKey;

impl<W: LayoutElement> ContainerTree<W> {
    /// Helper: get node key at path
    pub(super) fn get_node_key_at_path(&self, path: &[usize]) -> Option<NodeKey> {
        let root_key = self.root;
        self.node_at_branch_path(root_key, path)
    }

    /// Resolve a path read from a branch's own root — the addressing sway's `get_tree`
    /// publishes, where `nodes` and each entry of `floating_nodes` are separate trees.
    pub(in crate::layout) fn node_at_branch_path(
        &self,
        branch_root: NodeKey,
        path: &[usize],
    ) -> Option<NodeKey> {
        let mut current_key = branch_root;

        for &idx in path {
            match self.get_node(current_key)? {
                NodeData::Workspace(_) | NodeData::Container(_) => {
                    current_key = self.get_container(current_key)?.child_key(idx)?;
                }
                NodeData::Leaf(_) => return None,
            }
        }

        Some(current_key)
    }

    pub(super) fn leaf_under_key(&self, mut key: NodeKey) -> Option<NodeKey> {
        loop {
            match self.get_node(key)? {
                NodeData::Leaf(_) => return Some(key),
                NodeData::Workspace(_) | NodeData::Container(_) => {
                    if self.get_container(key)?.children.is_empty() {
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

    /// The first leaf of one branch.
    pub(in crate::layout) fn first_leaf_in_branch(&self, branch_root: NodeKey) -> Option<NodeKey> {
        self.leaf_under_key(branch_root)
    }

    /// sway's `seat_set_focus`: focus this node, whatever kind it is.
    ///
    /// What is raised is what was focused. Focusing a container raises the container, and the
    /// keyboard follows it down to a view the way sway's does — through the order, without
    /// disturbing it. Raising the leaf instead put a window nobody had focused ahead of one
    /// somebody had, and every later descent into that subtree answered with it.
    pub(super) fn focus_node_key(&mut self, key: NodeKey) {
        let Some(leaf_key) = self.leaf_under_key(key) else {
            self.seat.clear();
            return;
        };
        let chain = self.focus_chain(key);
        self.seat.focus(&chain, Some(leaf_key));
        self.refresh_focus_visibility();
    }

    /// Move logical/seat focus without changing the active branch of any switcher container.
    ///
    /// This deliberately is not a general focus operation. Sway uses it for one move outcome:
    /// the moved view keeps seat focus while the sibling tab/stack selected during tree surgery
    /// remains visible.
    pub(super) fn set_seat_focus_preserving_switcher(&mut self, leaf_key: NodeKey) {
        if matches!(self.get_node(leaf_key), Some(NodeData::Leaf(_))) {
            self.seat.follow_without_raising(leaf_key);
        }
    }

    /// A node's path within its own branch.
    ///
    /// Addresses are relative to a root, and there is more than one root: the workspace's, and
    /// one per floating group. That is not a convention invented here — sway's `get_tree`
    /// gives a workspace `nodes` and `floating_nodes` as separate arrays, and `LayoutTree` has
    /// had the same two sides all along. This is the one lookup that knows both.
    pub(in crate::layout) fn branch_relative_path(
        &self,
        target_key: NodeKey,
    ) -> Option<Vec<usize>> {
        let branch_root = self.branch_root(target_key);
        if target_key == branch_root {
            return self.nodes.contains_key(target_key).then(Vec::new);
        }

        let mut path_rev = Vec::new();
        let mut current = target_key;
        while current != branch_root {
            let parent = self.parent_of(current)?;
            path_rev.push(self.child_index(parent, current)?);
            current = parent;
        }
        path_rev.reverse();
        Some(path_rev)
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
        // Focus history is the seat's order, kept by SeatFocus.
    }

    /// Find a window by ID and return its node key.
    ///
    /// Internal callers use the stable identity directly rather than materializing a path only
    /// to resolve it back into the key they actually wanted.
    pub(in crate::layout) fn window_key(&self, window_id: &W::Id) -> Option<NodeKey> {
        self.nodes.iter().find_map(|(key, node)| match node {
            NodeData::Leaf(tile) if tile.window().id() == window_id => Some(key),
            _ => None,
        })
    }
}
