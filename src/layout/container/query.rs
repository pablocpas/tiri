//! Read-only traversals and collection queries over the tree.

use std::collections::HashMap;

use smithay::utils::Logical;
use smithay::utils::Rectangle;

use super::ContainerData;
use super::ContainerTree;
use super::Layout;
use super::LayoutElement;
use super::NodeData;
use super::NodeKey;
use super::ResizeReach;
use crate::layout::tile::Tile;

impl<W: LayoutElement> ContainerTree<W> {
    /// Get all windows in the tree (depth-first traversal)
    pub(in crate::layout) fn all_windows(&self) -> Vec<&W> {
        let mut windows = Vec::new();
        self.collect_windows_from_node(self.root, &mut windows);
        windows
    }

    /// The windows of one branch — the tiled side, or one floating group.
    ///
    /// The unqualified `all_windows` means the tiled side, because that is what every caller
    /// meant when there was only one side to mean. Now that the floating groups are branches
    /// of the same tree, asking for "all" has to say all of what.
    pub(in crate::layout) fn windows_in_branch(&self, branch_root: NodeKey) -> Vec<&W> {
        let mut windows = Vec::new();
        self.collect_windows_from_node(branch_root, &mut windows);
        windows
    }

    /// The tiles of one branch.
    pub(in crate::layout) fn tiles_in_branch(&self, branch_root: NodeKey) -> Vec<&Tile<W>> {
        let mut tiles = Vec::new();
        self.collect_tiles_from_node(branch_root, &mut tiles);
        tiles
    }

    /// The leaves of one branch.
    pub(in crate::layout) fn leaf_keys_in_branch(&self, branch_root: NodeKey) -> Vec<NodeKey> {
        let mut keys = Vec::new();
        self.collect_leaf_keys(branch_root, &mut keys);
        keys
    }

    /// Helper: collect all windows from a node
    pub(super) fn collect_windows_from_node<'a>(
        &'a self,
        node_key: NodeKey,
        windows: &mut Vec<&'a W>,
    ) {
        match self.get_node(node_key) {
            Some(NodeData::Leaf(tile)) => windows.push(tile.window()),
            Some(NodeData::Container(container)) => {
                for &child_key in &container.children {
                    self.collect_windows_from_node(child_key, windows);
                }
            }
            None => {}
        }
    }

    /// All window IDs in the tree, depth-first.
    #[cfg(test)]
    pub(in crate::layout) fn all_window_ids(&self) -> Vec<W::Id> {
        self.window_ids_under(self.root)
    }

    /// Window IDs in the subtree rooted at `key`, depth-first.
    pub(in crate::layout) fn window_ids_under(&self, key: NodeKey) -> Vec<W::Id> {
        let mut ids = Vec::new();
        self.collect_window_ids_from_node(key, &mut ids);
        ids
    }

    pub(super) fn collect_window_ids_from_node(&self, node_key: NodeKey, ids: &mut Vec<W::Id>) {
        match self.get_node(node_key) {
            Some(NodeData::Leaf(tile)) => ids.push(tile.window().id().clone()),
            Some(NodeData::Container(container)) => {
                for &child_key in &container.children {
                    self.collect_window_ids_from_node(child_key, ids);
                }
            }
            None => {}
        }
    }

    /// Get all tiles in the tree (depth-first traversal)
    pub(in crate::layout) fn all_tiles(&self) -> Vec<&Tile<W>> {
        let mut tiles = Vec::new();
        self.collect_tiles_from_node(self.root, &mut tiles);
        tiles
    }

    /// Helper: collect all tiles from a node
    pub(super) fn collect_tiles_from_node<'a>(
        &'a self,
        node_key: NodeKey,
        tiles: &mut Vec<&'a Tile<W>>,
    ) {
        match self.get_node(node_key) {
            Some(NodeData::Leaf(tile)) => tiles.push(tile),
            Some(NodeData::Container(container)) => {
                for &child_key in &container.children {
                    self.collect_tiles_from_node(child_key, tiles);
                }
            }
            None => {}
        }
    }

    /// Collect raw pointers to tiles (immutable) in depth-first order.
    /// Leaf node keys in depth-first (visual) order.
    /// Every leaf the tree holds, on both sides.
    ///
    /// Both, deliberately. This is what the transaction machinery compares its snapshot
    /// against, and a floating window that the comparison cannot see is a window whose
    /// configure gets computed from a tree that does not contain it.
    pub(in crate::layout) fn dfs_leaf_keys(&self) -> Vec<NodeKey> {
        let mut keys = Vec::new();
        self.collect_leaf_keys(self.root, &mut keys);
        for floating_root in self.floating_roots().collect::<Vec<_>>() {
            self.collect_leaf_keys(floating_root, &mut keys);
        }
        keys
    }

    fn collect_leaf_keys(&self, node_key: NodeKey, out: &mut Vec<NodeKey>) {
        match self.get_node(node_key) {
            Some(NodeData::Leaf(_)) => out.push(node_key),
            Some(NodeData::Container(container)) => {
                for &child_key in &container.children {
                    self.collect_leaf_keys(child_key, out);
                }
            }
            None => {}
        }
    }

    /// Whether any container in the tree uses `layout`.
    ///
    /// Tests use this instead of searching the debug dump for a layout name, which also
    /// matches window titles and silently changes meaning with the dump's format.
    #[cfg(test)]
    /// Whether some container inside the workspace carries `layout`.
    ///
    /// The workspace itself does not count. It always exists and always has a layout, so
    /// including it would answer "yes" to a question that is asking whether a *container was
    /// built* — which is what every caller wants to know. Its own layout is
    /// [`Self::workspace_layout`].
    pub(in crate::layout) fn contains_layout(&self, layout: Layout) -> bool {
        self.nodes.iter().any(|(key, node)| match node {
            NodeData::Container(container) => key != self.root && container.layout() == layout,
            NodeData::Leaf(_) => false,
        })
    }

    /// The focused window's id, if any.
    #[cfg(test)]
    pub(in crate::layout) fn focused_window_id(&self) -> Option<W::Id> {
        self.focused_window().map(|window| window.id().clone())
    }

    /// Leaf node keys under `key`, in depth-first (visual) order.
    pub(in crate::layout) fn leaf_keys_under(&self, key: NodeKey) -> Vec<NodeKey> {
        let mut keys = Vec::new();
        self.collect_leaf_keys(key, &mut keys);
        keys
    }

    /// Mutable tiles for `keys`, each tagged with the index of its key in `keys` and sorted
    /// by that index. Keys that are not (or no longer) leaves of this tree are skipped.
    pub(in crate::layout) fn tiles_mut_for_keys(
        &mut self,
        keys: &[NodeKey],
    ) -> Vec<(usize, &mut Tile<W>)> {
        let rank: HashMap<NodeKey, usize> = keys
            .iter()
            .enumerate()
            .map(|(idx, key)| (*key, idx))
            .collect();
        let mut out: Vec<(usize, &mut Tile<W>)> = self
            .nodes
            .iter_mut()
            .filter_map(|(key, node)| match node {
                NodeData::Leaf(tile) => rank.get(&key).map(|&idx| (idx, tile)),
                _ => None,
            })
            .collect();
        out.sort_by_key(|(idx, _)| *idx);
        out
    }

    /// Layout, geometry and child count of the container at `key`.
    pub(in crate::layout) fn container_info(
        &self,
        key: NodeKey,
    ) -> Option<(Layout, Rectangle<f64, Logical>, usize)> {
        let container = self.get_container(key)?;
        Some((
            container.layout(),
            container.geometry(),
            container.child_count(),
        ))
    }

    /// Whether the container at `key` is a container the user can address: it either holds
    /// several children or was created by an explicit split.
    pub(in crate::layout) fn container_is_meaningful_parent(&self, key: NodeKey) -> Option<bool> {
        let container = self.get_container(key)?;
        Some(container.child_count() > 1 || container.is_user_container())
    }

    /// Rect of the `child_idx`-th child of the container at `container_key`.
    pub(in crate::layout) fn child_rect_in(
        &self,
        container_key: NodeKey,
        child_idx: usize,
    ) -> Option<Rectangle<f64, Logical>> {
        let container = self.get_container(container_key)?;
        let child_key = container.child_key(child_idx)?;
        // Resize reads each sibling's settled pending box, not a fresh projection from its
        // fractions. Recomputing here normalizes once more than sway and can put a half-pixel
        // remainder on the opposite sibling; `container_resize_tiled` then snaps that wrong
        // pixel into the stored fractions.
        //
        // sway/commands/resize.c:117-163
        self.node_geometry(child_key)
    }

    /// Rect of a node, resolved through its parent container.
    pub(super) fn child_rect_for_key(&self, key: NodeKey) -> Option<Rectangle<f64, Logical>> {
        let parent_key = self.parent_of(key)?;
        let child_idx = self.child_index(parent_key, key)?;
        self.child_rect_in(parent_key, child_idx)
    }

    /// Nearest ancestor container of `key` whose layout is `layout`, plus the index of the
    /// branch of `key` inside it.
    pub(in crate::layout) fn find_parent_with_layout(
        &self,
        key: NodeKey,
        layout: Layout,
    ) -> Option<(NodeKey, usize)> {
        let mut current = key;
        while let Some(parent_key) = self.parent_of(current) {
            if let Some(container) = self.get_container(parent_key) {
                if container.layout() == layout {
                    let child_idx = self.child_index(parent_key, current)?;
                    return Some((parent_key, child_idx));
                }
            }
            current = parent_key;
        }

        None
    }

    /// Nearest ancestor split that can actually pay for a resize with `reach`.
    ///
    /// sway keeps climbing past same-axis containers with one child, and past an edge where
    /// the selected branch is already first or last. Stopping at either would turn a resize
    /// that belongs to an outer split into a no-op.
    pub(in crate::layout) fn find_resize_parent(
        &self,
        key: NodeKey,
        layout: Layout,
        reach: ResizeReach,
    ) -> Option<(NodeKey, usize)> {
        let mut current = key;
        while let Some(parent_key) = self.parent_of(current) {
            if let Some(container) = self.get_container(parent_key) {
                if container.layout() == layout {
                    let child_idx = self.child_index(parent_key, current)?;
                    let child_count = container.child_count();
                    let has_payer = match reach {
                        ResizeReach::Siblings => child_count > 1,
                        ResizeReach::Before => child_idx > 0,
                        ResizeReach::After => child_idx + 1 < child_count,
                    };
                    if has_payer {
                        return Some((parent_key, child_idx));
                    }
                }
            }
            current = parent_key;
        }

        None
    }

    /// Mutable access to the container at `key`.
    pub(in crate::layout) fn container_mut(&mut self, key: NodeKey) -> Option<&mut ContainerData> {
        self.get_container_mut(key)
    }

    /// Collect leaf paths under a given prefix path.
    pub(in crate::layout) fn leaf_paths_under(&self, prefix: &[usize]) -> Vec<Vec<usize>> {
        let mut results = Vec::new();
        let mut path = prefix.to_vec();
        if let Some(node_key) = self.get_node_key_at_path(prefix) {
            self.collect_leaf_paths_from_node(node_key, &mut path, &mut results);
        }
        results
    }

    pub(super) fn collect_leaf_paths_from_node(
        &self,
        node_key: NodeKey,
        path: &mut Vec<usize>,
        results: &mut Vec<Vec<usize>>,
    ) {
        match self.get_node(node_key) {
            Some(NodeData::Leaf(_)) => results.push(path.clone()),
            Some(NodeData::Container(container)) => {
                for (idx, &child_key) in container.children.iter().enumerate() {
                    path.push(idx);
                    self.collect_leaf_paths_from_node(child_key, path, results);
                    path.pop();
                }
            }
            None => {}
        }
    }
}
