//! Detaching and reattaching subtrees (window removal, float/unfloat surgery).

use std::collections::HashMap;

use super::ContainerData;
use super::ContainerTree;
use super::DetachedContainer;
use super::DetachedNode;
use super::InsertParentInfo;
use super::LayoutElement;
use super::NodeData;
use super::NodeKey;
use crate::layout::tile::Tile;

impl<W: LayoutElement> ContainerTree<W> {
    /// Remove a window by ID, returns the removed tile
    pub(in crate::layout) fn remove_window(&mut self, window_id: &W::Id) -> Option<Tile<W>> {
        let path = self.find_window(window_id)?;
        let node_key = self.get_node_key_at_path(&path)?;
        let cleanup_key = self.parent_of(node_key);
        let was_focused = self.focused_key == Some(node_key);

        // First, remove from parent's children list BEFORE removing from slotmap
        if !path.is_empty() {
            let parent_path = &path[..path.len() - 1];
            let child_idx = *path.last().unwrap();

            if let Some(parent_key) = self.get_node_key_at_path(parent_path) {
                if let Some(container) = self.get_container_mut(parent_key) {
                    container.remove_child(child_idx);
                }
            }
            self.set_parent(node_key, None);
        } else {
            // Was root
            self.root = None;
            self.set_parent(node_key, None);
        }

        // Now remove from slotmap (only the leaf, not recursive)
        let node_data = self.nodes.remove(node_key)?;
        self.parents.remove(node_key);
        let tile = match node_data {
            NodeData::Leaf(tile) => tile,
            NodeData::Container(_) => return None, // Should never happen
        };

        self.cleanup_containers(cleanup_key);
        self.prune_leaf_layouts();

        self.prune_selected_key();
        self.reconcile_focus_after_change(was_focused);

        self.layout();

        Some(tile)
    }

    pub(in crate::layout) fn take_subtree_at_path(
        &mut self,
        path: &[usize],
    ) -> Option<(DetachedNode<W>, Option<InsertParentInfo>)> {
        let node_key = self.get_node_key_at_path(path)?;
        let insert_info = self.insert_parent_info_for_path(path);

        let focused_path = self.focus_path();
        let focused_in_subtree =
            focused_path.len() >= path.len() && focused_path[..path.len()] == *path;

        if let Some(selected_key) = self.selected_key {
            if let Some(selected_path) = self.find_node_path(selected_key) {
                if selected_path.len() >= path.len() && selected_path[..path.len()] == *path {
                    self.selected_key = None;
                }
            }
        }

        let cleanup_key = if path.is_empty() {
            self.root = None;
            self.set_parent(node_key, None);
            None
        } else {
            let parent_path = &path[..path.len() - 1];
            let parent_key = if parent_path.is_empty() {
                self.root?
            } else {
                self.get_node_key_at_path(parent_path)?
            };

            if let Some(container) = self.get_container_mut(parent_key) {
                let idx = *path.last().unwrap();
                container.remove_child(idx);
            }
            self.set_parent(node_key, None);
            Some(parent_key)
        };

        let subtree = self.extract_subtree(node_key);
        self.cleanup_containers(cleanup_key);
        self.prune_leaf_layouts();

        self.prune_selected_key();
        self.reconcile_focus_after_change(focused_in_subtree);

        self.layout();

        Some((subtree, insert_info))
    }

    /// Extract a subtree rooted at the given key into a detached representation.
    pub(super) fn extract_subtree(&mut self, key: NodeKey) -> DetachedNode<W> {
        let node_data = self
            .nodes
            .remove(key)
            .expect("node key must exist when extracting subtree");
        self.parents.remove(key);

        match node_data {
            NodeData::Leaf(tile) => DetachedNode::Leaf(tile),
            NodeData::Container(container) => {
                let child_keys = container.children.clone();
                let mut children = Vec::new();
                for child_key in container.children {
                    children.push(self.extract_subtree(child_key));
                }
                let mut index_by_key = HashMap::new();
                for (idx, key) in child_keys.iter().enumerate() {
                    index_by_key.insert(*key, idx);
                }
                let focus_stack = container
                    .focus_stack
                    .iter()
                    .filter_map(|key| index_by_key.get(key).copied())
                    .collect();
                DetachedNode::Container(DetachedContainer::from_parts(
                    container.layout,
                    children,
                    container.child_percents,
                    focus_stack,
                    container.preserve_on_single,
                    container.prev_split_layout,
                ))
            }
        }
    }

    /// Insert a detached subtree into this tree, returning the new root key.
    pub(super) fn insert_subtree(&mut self, subtree: DetachedNode<W>) -> NodeKey {
        match subtree {
            DetachedNode::Leaf(tile) => self.insert_node(NodeData::Leaf(tile)),
            DetachedNode::Container(container) => {
                let container_key =
                    self.insert_node(NodeData::Container(ContainerData::new(container.layout)));

                let mut child_keys = Vec::new();
                for child in container.children {
                    let child_key = self.insert_subtree(child);
                    self.set_parent(child_key, Some(container_key));
                    child_keys.push(child_key);
                }

                if let Some(node) = self.get_container_mut(container_key) {
                    node.children = child_keys;
                    node.child_percents = container.child_percents;
                    node.focus_stack = container
                        .focus_stack
                        .iter()
                        .filter_map(|idx| node.children.get(*idx).copied())
                        .collect();
                    node.preserve_on_single = container.preserve_on_single;
                    node.prev_split_layout = container.prev_split_layout;
                    if node.child_percents.len() != node.children.len() {
                        node.recalculate_percentages();
                    } else {
                        node.normalize_child_percents();
                    }
                    node.ensure_focus_stack();
                }

                container_key
            }
        }
    }

    /// Remove and return the root-level child at the given index as a detached subtree.
    pub(in crate::layout) fn take_root_child_subtree(
        &mut self,
        idx: usize,
    ) -> Option<DetachedNode<W>> {
        let root_key = self.root?;

        match self.get_node(root_key) {
            Some(NodeData::Leaf(_)) => {
                if idx == 0 {
                    self.focused_key = None;
                    self.selected_key = None;
                    let subtree = self.extract_subtree(root_key);
                    self.root = None;
                    self.prune_leaf_layouts();
                    Some(subtree)
                } else {
                    None
                }
            }
            Some(NodeData::Container(_)) => {
                let child_key = {
                    let container = self.get_container(root_key)?;
                    if idx >= container.children.len() {
                        return None;
                    }
                    container.child_key(idx)?
                };

                if let Some(container) = self.get_container_mut(root_key) {
                    container.remove_child(idx);
                }
                self.set_parent(child_key, None);

                let remaining = self.get_container(root_key)?.children.len();

                self.cleanup_containers(Some(root_key));
                self.prune_leaf_layouts();

                match self.get_node(root_key) {
                    Some(NodeData::Leaf(_)) | None => {
                        self.focus_first_leaf();
                    }
                    Some(NodeData::Container(root_container)) => {
                        if remaining > 0 {
                            let new_idx = idx.min(root_container.children.len().saturating_sub(1));
                            let child_key = root_container.child_key(new_idx);
                            if let Some(child_key) = child_key {
                                self.focus_node_key(child_key);
                            } else {
                                self.focus_first_leaf();
                            }
                        } else {
                            self.focus_first_leaf();
                        }
                    }
                }

                let subtree = self.extract_subtree(child_key);
                self.prune_selected_key();
                Some(subtree)
            }
            None => None,
        }
    }
}
