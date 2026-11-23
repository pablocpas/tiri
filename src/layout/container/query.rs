//! Read-only traversals and collection queries over the tree.

use std::collections::HashMap;

use smithay::utils::Logical;
use smithay::utils::Rectangle;

use super::ContainerArena;
use super::Layout;
use super::LayoutElement;
use super::NodeData;
use super::NodeKey;
use super::ResizeReach;
use crate::layout::tile::Tile;

impl<W: LayoutElement> ContainerArena<W> {
    fn visit_leaves<'a>(&'a self, node_key: NodeKey, visit: &mut impl FnMut(NodeKey, &'a Tile<W>)) {
        match self.get_node(node_key) {
            Some(node) if node.is_view() => {
                let tile = node.as_tile().expect("a view holds a tile");
                visit(node_key, tile);
            }
            Some(NodeData::Workspace(_)) | Some(NodeData::Container(_)) => {
                for &child_key in &self
                    .get_container(node_key)
                    .expect("layout parent")
                    .children
                {
                    self.visit_leaves(child_key, visit);
                }
            }
            None => {}
        }
    }

    /// Get all windows in the tree (depth-first traversal)
    pub(in crate::layout) fn all_windows(&self) -> Vec<&W> {
        self.windows_in_branch(self.root)
    }

    /// The windows of one branch — the tiled side, or one floating group.
    ///
    /// The unqualified `all_windows` means the tiled side, because that is what every caller
    /// meant when there was only one side to mean. Now that the floating groups are branches
    /// of the same tree, asking for "all" has to say all of what.
    pub(in crate::layout) fn windows_in_branch(&self, branch_root: NodeKey) -> Vec<&W> {
        let mut windows = Vec::new();
        self.visit_leaves(branch_root, &mut |_, tile| windows.push(tile.window()));
        windows
    }

    /// The tiles of one branch.
    pub(in crate::layout) fn tiles_in_branch(&self, branch_root: NodeKey) -> Vec<&Tile<W>> {
        let mut tiles = Vec::new();
        self.visit_leaves(branch_root, &mut |_, tile| tiles.push(tile));
        tiles
    }

    /// The leaves of one branch.
    pub(in crate::layout) fn leaf_keys_in_branch(&self, branch_root: NodeKey) -> Vec<NodeKey> {
        self.leaf_keys_under(branch_root)
    }

    /// All window IDs in the tree, depth-first.
    #[cfg(test)]
    pub(in crate::layout) fn all_window_ids(&self) -> Vec<W::Id> {
        self.window_ids_under(self.root)
    }

    /// Window IDs in the subtree rooted at `key`, depth-first.
    pub(in crate::layout) fn window_ids_under(&self, key: NodeKey) -> Vec<W::Id> {
        let mut ids = Vec::new();
        self.visit_leaves(key, &mut |_, tile| ids.push(tile.window().id().clone()));
        ids
    }

    /// Get all tiles in the tree (depth-first traversal)
    pub(in crate::layout) fn all_tiles(&self) -> Vec<&Tile<W>> {
        self.tiles_in_branch(self.root)
    }

    /// Every tile currently stored in this arena (tiled and floating), in arbitrary order.
    ///
    /// Use this when order does not matter (config reload, shader reload, animation ticks)
    /// instead of collecting DFS keys and sorting mutable borrows.
    pub(in crate::layout) fn every_tile(&self) -> impl Iterator<Item = &Tile<W>> + '_ {
        self.nodes.iter().filter_map(|(_, node)| node.as_tile())
    }

    /// Mutable counterpart to [`Self::every_tile`].
    pub(in crate::layout) fn every_tile_mut(&mut self) -> impl Iterator<Item = &mut Tile<W>> + '_ {
        self.nodes
            .iter_mut()
            .filter_map(|(_, node)| node.as_tile_mut())
    }

    /// Every leaf the tree holds, on both sides, in depth-first (visual) order.
    ///
    /// Both, deliberately. This is what the transaction machinery compares its snapshot
    /// against, and a floating window that the comparison cannot see is a window whose
    /// configure gets computed from a tree that does not contain it.
    pub(in crate::layout) fn dfs_leaf_keys(&self) -> Vec<NodeKey> {
        let mut keys = Vec::new();
        self.visit_leaves(self.root, &mut |key, _| keys.push(key));
        for floating_root in self.floating_roots() {
            self.visit_leaves(floating_root, &mut |key, _| keys.push(key));
        }
        keys
    }

    /// Whether some container inside the workspace carries `layout`.
    ///
    /// The workspace itself does not count. It always exists and always has a layout, so
    /// including it would answer "yes" to a question that is asking whether a *container was
    /// built* — which is what every caller wants to know. Its own layout is
    /// [`Self::workspace_layout`].
    #[cfg(test)]
    pub(in crate::layout) fn contains_layout(&self, layout: Layout) -> bool {
        self.nodes.iter().any(|(key, node)| match node {
            NodeData::Container(container) if !container.is_view() => {
                key != self.root && container.layout() == layout
            }
            NodeData::Workspace(_) | NodeData::Container(_) => false,
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
        self.visit_leaves(key, &mut |leaf_key, _| keys.push(leaf_key));
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
            .filter_map(|(key, node)| {
                node.as_tile_mut()
                    .and_then(|tile| rank.get(&key).map(|&idx| (idx, tile)))
            })
            .collect();
        out.sort_by_key(|(idx, _)| *idx);
        out
    }

    /// Layout, geometry and child count of a workspace or container node.
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
            Some(node) if node.is_view() => results.push(path.clone()),
            Some(NodeData::Workspace(_)) | Some(NodeData::Container(_)) => {
                let container = self.get_container(node_key).expect("layout parent");
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
