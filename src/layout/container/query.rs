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

    /// Get window IDs under the subtree at `path` (depth-first traversal).
    ///
    /// An empty `path` means the whole tree.
    pub(in crate::layout) fn window_ids_under_path(&self, path: &[usize]) -> Vec<W::Id> {
        let mut ids = Vec::new();
        let node_key = if path.is_empty() {
            self.root
        } else {
            self.get_node_key_at_path(path)
        };
        let Some(node_key) = node_key else {
            return ids;
        };
        self.collect_window_ids_from_node(node_key, &mut ids);
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

    /// Helper: get tile at a given path (immutable).
    pub(in crate::layout) fn tile_at_path(&self, path: &[usize]) -> Option<&Tile<W>> {
        let key = self.get_node_key_at_path(path)?;
        self.get_tile(key)
    }

    /// Helper: get tile at a given path (mutable).
    pub(in crate::layout) fn tile_at_path_mut(&mut self, path: &[usize]) -> Option<&mut Tile<W>> {
        let key = self.get_node_key_at_path(path)?;
        self.get_tile_mut(key)
    }

    pub(in crate::layout) fn container_info(
        &self,
        path: &[usize],
    ) -> Option<(Layout, Rectangle<f64, Logical>, usize)> {
        let container_key = if path.is_empty() {
            self.root?
        } else {
            self.get_node_key_at_path(path)?
        };

        let container = self.get_container(container_key)?;
        Some((
            container.layout(),
            container.geometry(),
            container.child_count(),
        ))
    }

    pub(in crate::layout) fn container_is_meaningful_parent(&self, path: &[usize]) -> Option<bool> {
        let container_key = if path.is_empty() {
            self.root?
        } else {
            self.get_node_key_at_path(path)?
        };

        let container = self.get_container(container_key)?;
        Some(container.child_count() > 1 || container.preserve_on_single())
    }

    pub(in crate::layout) fn child_rect_at(
        &self,
        parent_path: &[usize],
        child_idx: usize,
    ) -> Option<Rectangle<f64, Logical>> {
        let container_key = if parent_path.is_empty() {
            self.root?
        } else {
            self.get_node_key_at_path(parent_path)?
        };

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

    pub(in crate::layout) fn find_parent_with_layout(
        &self,
        mut path: Vec<usize>,
        layout: Layout,
    ) -> Option<(Vec<usize>, usize)> {
        while !path.is_empty() {
            let child_idx = *path.last().unwrap();
            let parent_path_vec = path[..path.len() - 1].to_vec();

            let container_key = if parent_path_vec.is_empty() {
                self.root?
            } else {
                self.get_node_key_at_path(&parent_path_vec)?
            };

            if let Some(container) = self.get_container(container_key) {
                if container.layout() == layout {
                    return Some((parent_path_vec, child_idx));
                }
            }

            path.pop();
        }

        None
    }

    pub(in crate::layout) fn container_at_path_mut(
        &mut self,
        path: &[usize],
    ) -> Option<&mut ContainerData> {
        let key = if path.is_empty() {
            self.root?
        } else {
            self.get_node_key_at_path(path)?
        };
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
