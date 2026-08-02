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
use crate::layout::tile::Tile;

impl<W: LayoutElement> ContainerTree<W> {
    /// Get all windows in the tree (depth-first traversal)
    pub(in crate::layout) fn all_windows(&self) -> Vec<&W> {
        let mut windows = Vec::new();
        if let Some(root_key) = self.root {
            self.collect_windows_from_node(root_key, &mut windows);
        }
        windows
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
    pub(in crate::layout) fn all_window_ids(&self) -> Vec<W::Id> {
        match self.root {
            Some(root_key) => self.window_ids_under(root_key),
            None => Vec::new(),
        }
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
        if let Some(root_key) = self.root {
            self.collect_tiles_from_node(root_key, &mut tiles);
        }
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
    pub(super) fn dfs_leaf_keys(&self) -> Vec<NodeKey> {
        let mut keys = Vec::new();
        if let Some(root_key) = self.root {
            self.collect_leaf_keys(root_key, &mut keys);
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
    pub(in crate::layout) fn contains_layout(&self, layout: Layout) -> bool {
        self.nodes.values().any(|node| match node {
            NodeData::Container(container) => container.layout() == layout,
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

    /// All tiles mutably, in depth-first (visual) order.
    pub(in crate::layout) fn all_tiles_mut(&mut self) -> Vec<&mut Tile<W>> {
        let keys = self.dfs_leaf_keys();
        self.tiles_mut_for_keys(&keys)
            .into_iter()
            .map(|(_, tile)| tile)
            .collect()
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

    /// Layout, geometry and child count of the root, when the root is a container.
    pub(in crate::layout) fn root_info(&self) -> Option<(Layout, Rectangle<f64, Logical>, usize)> {
        self.container_info(self.root?)
    }

    /// Whether the root is a container the user can address. None when there is no root or
    /// the root is a bare leaf.
    pub(in crate::layout) fn root_is_meaningful_parent(&self) -> Option<bool> {
        self.container_is_meaningful_parent(self.root?)
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
        if child_idx >= container.child_count() {
            return None;
        }

        let child_key = container.child_key(child_idx)?;
        let child_is_leaf = matches!(self.get_node(child_key), Some(NodeData::Leaf(_)));
        let child_count = container.child_count();
        let percents_sum: f64 = container.child_percents_slice().iter().copied().sum();
        let percents = self.get_normalized_child_percents(container_key, child_count, percents_sum);
        let (rect, _) = self.preview_child_rect(
            container.layout(),
            container.geometry(),
            child_count,
            &percents,
            child_idx,
            child_is_leaf,
        );

        Some(rect)
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
