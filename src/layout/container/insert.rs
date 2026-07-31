//! Window and subtree insertion at focus, path or split targets.

use super::ContainerData;
use super::ContainerTree;
use super::DetachedNode;
use super::Direction;
use super::InsertParentInfo;
use super::Layout;
use super::LayoutElement;
use super::NodeData;
use super::NodeKey;
use crate::layout::tile::Tile;

impl<W: LayoutElement> ContainerTree<W> {
    /// Insert a window into the tree, focusing it afterwards.
    #[cfg(test)]
    pub(in crate::layout) fn insert_window(&mut self, tile: Tile<W>) {
        self.insert_window_with_focus(tile, true);
    }

    /// Insert a window into the tree, optionally focusing it afterwards.
    pub(in crate::layout) fn insert_window_with_focus(&mut self, tile: Tile<W>, focus: bool) {
        self.clear_focus_history();

        if self.root.is_none() {
            let tile_key = self.insert_node(NodeData::Leaf(tile));

            if self.pending_layout_wrap_on_split {
                let layout = self.pending_layout.take().unwrap_or(Layout::SplitH);
                self.pending_layout_wrap_on_split = false;

                let mut container = ContainerData::new(layout);
                container.mark_preserve_on_single();
                container.add_child(tile_key);

                let container_key = self.insert_node(NodeData::Container(container));
                self.set_parent(tile_key, Some(container_key));
                self.set_parent(container_key, None);
                self.root = Some(container_key);
            } else {
                // Match i3/sway: layout commands issued on an empty workspace apply when a
                // second window arrives (root-leaf conversion), not to the first opened window.
                self.set_parent(tile_key, None);
                self.root = Some(tile_key);
            }

            self.settle_focus_after_insert(tile_key, focus);
            return;
        }

        let tile_key = self.insert_node(NodeData::Leaf(tile));
        self.insert_key_as_focus_sibling(tile_key, focus);
    }

    /// Insert a detached subtree into the tree, optionally focusing it afterwards.
    pub(in crate::layout) fn insert_subtree_with_focus(
        &mut self,
        subtree: DetachedNode<W>,
        focus: bool,
    ) {
        self.clear_focus_history();

        let node_key = self.insert_subtree(subtree);

        let tree_has_no_leaves = self.first_leaf_key().is_none();
        if self.root.is_none() || tree_has_no_leaves {
            if let Some(old_root) = self.root.take() {
                self.remove_node_recursive(old_root);
            }
            self.set_parent(node_key, None);
            self.root = Some(node_key);
            self.settle_focus_after_insert(node_key, focus);
            return;
        }

        self.insert_key_as_focus_sibling(node_key, focus);
    }

    /// Insert an already-materialized node as a sibling of the selected/focused node,
    /// following i3's focus-parent semantics. The tree must have a non-empty root.
    fn insert_key_as_focus_sibling(&mut self, node_key: NodeKey, focus: bool) {
        // Ensure the root is a container so we can insert siblings easily.
        let root_key = self.expect_root();
        if matches!(self.get_node(root_key), Some(NodeData::Leaf(_))) {
            // Convert the root leaf into a container.
            let old_root_key = self.take_root();
            let layout = self.pending_layout.take().unwrap_or(Layout::SplitH);
            self.pending_layout_wrap_on_split = false;
            let mut container = ContainerData::new(layout);
            container.add_child(old_root_key);

            let container_key = self.insert_node(NodeData::Container(container));
            self.set_parent(old_root_key, Some(container_key));
            self.set_parent(container_key, None);
            self.root = Some(container_key);
            self.focus_node_key(old_root_key);
        }

        self.ensure_selected_root_has_parent_for_sibling_insert();

        // Prefer the selected container/leaf target (focus-parent semantics): i3/sway insert
        // new windows as siblings of the selected node. Fall back to the focused leaf.
        let selected_target = self.selected_key.and_then(|selected_key| {
            self.get_node(selected_key)?;
            let parent_key = self.parent_of(selected_key)?;
            let selected_idx = self.child_index(parent_key, selected_key)?;
            Some((parent_key, selected_idx + 1))
        });

        let focus_target =
            self.focused_key
                .or_else(|| self.first_leaf_key())
                .and_then(|focused_key| {
                    let parent_key = self.parent_of(focused_key)?;
                    let focused_idx = self.child_index(parent_key, focused_key)?;
                    Some((parent_key, focused_idx + 1))
                });

        let insert_target = selected_target.or(focus_target).or_else(|| {
            let root_key = self.root?;
            let root = self.get_container(root_key)?;
            Some((root_key, root.child_count()))
        });

        if let Some((parent_key, insert_idx)) = insert_target {
            if let Some(NodeData::Container(parent_container)) = self.get_node_mut(parent_key) {
                parent_container.insert_child(insert_idx, node_key);
                self.set_parent(node_key, Some(parent_key));
                self.settle_focus_after_insert(node_key, focus);
                return;
            }
        }

        // Fallback: append to the root container.
        if let Some(root_key) = self.root {
            if let Some(NodeData::Container(container)) = self.get_node_mut(root_key) {
                let insert_idx = container.children.len();
                container.insert_child(insert_idx, node_key);
                self.set_parent(node_key, Some(root_key));
                self.settle_focus_after_insert(node_key, focus);
            }
        }
    }

    pub(in crate::layout) fn insert_leaf_after(
        &mut self,
        window_id: &W::Id,
        tile: Tile<W>,
        focus: bool,
    ) -> bool {
        let path = match self.find_window(window_id) {
            Some(path) => path,
            None => {
                self.append_leaf(tile, focus);
                return true;
            }
        };

        if path.is_empty() {
            self.append_leaf(tile, focus);
            return true;
        }

        let parent_path = &path[..path.len() - 1];
        let current_idx = *path.last().unwrap();

        let parent_key = if parent_path.is_empty() {
            self.root
        } else {
            self.get_node_key_at_path(parent_path)
        };

        if let Some(parent_key) = parent_key {
            let insert_idx = current_idx + 1;
            let tile_key = self.insert_node(NodeData::Leaf(tile));

            if let Some(parent) = self.get_container_mut(parent_key) {
                parent.insert_child(insert_idx, tile_key);
                self.set_parent(tile_key, Some(parent_key));
                self.settle_focus_after_insert(tile_key, focus);
                return true;
            }
        }

        false
    }

    pub(in crate::layout) fn insert_leaf_in_root_container(
        &mut self,
        root_idx: usize,
        tile_idx: Option<usize>,
        tile: Tile<W>,
        focus: bool,
    ) -> bool {
        let root_key = self.ensure_root_container();

        let root_container = match self.get_container(root_key) {
            Some(c) => c,
            None => return false,
        };

        if root_idx >= root_container.children.len() {
            return false;
        }

        let Some(root_child_key) = root_container.child_key(root_idx) else {
            return false;
        };

        if matches!(self.get_node(root_child_key), Some(NodeData::Leaf(_))) {
            // Wrap the root leaf child in a vertical container so tiles can stack inside it.
            let mut wrapper = ContainerData::new(Layout::SplitV);
            wrapper.add_child(root_child_key);
            let wrapper_key = self.insert_node(NodeData::Container(wrapper));
            self.set_parent(root_child_key, Some(wrapper_key));

            let Some(root_container) = self.get_container_mut(root_key) else {
                return false;
            };
            root_container.replace_child_preserving_focus(root_child_key, wrapper_key);
            self.set_parent(wrapper_key, Some(root_key));
        }

        // Now insert the new tile.
        let root_child_key = match self.get_container(root_key) {
            Some(c) => match c.child_key(root_idx) {
                Some(key) => key,
                None => return false,
            },
            None => return false,
        };
        let root_child_container = match self.get_container(root_child_key) {
            Some(c) => c,
            None => return false,
        };

        let insert_at = tile_idx.unwrap_or(root_child_container.children.len());
        let insert_at = insert_at.min(root_child_container.children.len());

        let tile_key = self.insert_node(NodeData::Leaf(tile));

        if let Some(root_child_container) = self.get_container_mut(root_child_key) {
            root_child_container.insert_child(insert_at, tile_key);
            self.settle_focus_after_insert(tile_key, focus);
        }
        self.set_parent(tile_key, Some(root_child_key));

        true
    }

    pub(in crate::layout) fn insert_parent_info_for_window(
        &self,
        window_id: &W::Id,
    ) -> Option<InsertParentInfo> {
        let path = self.find_window(window_id)?;
        self.insert_parent_info_for_path(&path)
    }

    pub(super) fn insert_parent_info_for_path(&self, path: &[usize]) -> Option<InsertParentInfo> {
        if path.is_empty() {
            return None;
        }

        let mut parent_path = path.to_vec();
        let insert_idx = parent_path.pop().unwrap();
        let parent_key = if parent_path.is_empty() {
            self.root?
        } else {
            self.get_node_key_at_path(&parent_path)?
        };
        let parent = self.get_container(parent_key)?;
        Some(InsertParentInfo {
            parent_path,
            insert_idx,
            layout: parent.layout(),
            child_percents: parent.child_percents_slice().to_vec(),
        })
    }

    pub(in crate::layout) fn replace_leaf_at_path(
        &mut self,
        path: &[usize],
        tile: Tile<W>,
    ) -> Option<Tile<W>> {
        let key = self.get_node_key_at_path(path)?;
        match self.get_node_mut(key)? {
            NodeData::Leaf(existing) => Some(std::mem::replace(existing, tile)),
            _ => None,
        }
    }

    pub(in crate::layout) fn is_leaf_at_path(&self, path: &[usize]) -> bool {
        let Some(key) = self.get_node_key_at_path(path) else {
            return false;
        };
        matches!(self.get_node(key), Some(NodeData::Leaf(_)))
    }

    pub(in crate::layout) fn insert_leaf_with_parent_info(
        &mut self,
        info: &InsertParentInfo,
        tile: Tile<W>,
        focus: bool,
    ) -> bool {
        let tile_key = self.insert_node(NodeData::Leaf(tile));
        self.insert_key_with_parent_info(info, tile_key, focus)
    }

    pub(in crate::layout) fn insert_subtree_with_parent_info(
        &mut self,
        info: &InsertParentInfo,
        subtree: DetachedNode<W>,
        focus: bool,
    ) -> bool {
        let node_key = self.insert_subtree(subtree);
        self.insert_key_with_parent_info(info, node_key, focus)
    }

    /// Insert an already-materialized node at the container described by `info`,
    /// restoring the recorded child percents when they still apply.
    fn insert_key_with_parent_info(
        &mut self,
        info: &InsertParentInfo,
        node_key: NodeKey,
        focus: bool,
    ) -> bool {
        let container_key = match self.ensure_container_at_path(&info.parent_path, info.layout) {
            Some(key) => key,
            None => {
                self.insert_key_at_root(self.root_children_len(), node_key, focus);
                return true;
            }
        };

        if let Some(container) = self.get_container_mut(container_key) {
            container.insert_child(info.insert_idx, node_key);
            if info.child_percents.len() == container.child_percents.len() {
                container.child_percents = info.child_percents.clone();
                container.normalize_child_percents();
            }
        }
        self.set_parent(node_key, Some(container_key));

        self.settle_focus_after_insert(node_key, focus);

        true
    }

    /// Create a new preserve-on-single split container along `direction` holding `existing`
    /// and `new_key`, ordered so that `new_key` sits on the side `direction` points to.
    fn new_split_pair_container(
        &mut self,
        existing: NodeKey,
        new_key: NodeKey,
        direction: Direction,
    ) -> NodeKey {
        let mut container = ContainerData::new(direction.split_layout());
        container.mark_preserve_on_single();
        if direction.is_leading() {
            container.add_child(new_key);
            container.add_child(existing);
        } else {
            container.add_child(existing);
            container.add_child(new_key);
        }
        let container_key = self.insert_node(NodeData::Container(container));
        self.set_parent(new_key, Some(container_key));
        self.set_parent(existing, Some(container_key));
        container_key
    }

    pub(in crate::layout) fn insert_leaf_split(
        &mut self,
        target_path: &[usize],
        direction: Direction,
        tile: Tile<W>,
        focus: bool,
    ) -> bool {
        if self.root.is_none() {
            self.append_leaf(tile, focus);
            return true;
        }

        let desired_layout = direction.split_layout();

        if target_path.is_empty() {
            let Some(root_key) = self.root else {
                self.append_leaf(tile, focus);
                return true;
            };
            if !matches!(self.get_node(root_key), Some(NodeData::Leaf(_))) {
                self.append_leaf(tile, focus);
                return true;
            }

            let tile_key = self.insert_node(NodeData::Leaf(tile));
            let container_key = self.new_split_pair_container(root_key, tile_key, direction);
            self.set_parent(container_key, None);
            self.root = Some(container_key);

            self.settle_focus_after_insert(tile_key, focus);
            return true;
        }

        let parent_path = &target_path[..target_path.len() - 1];
        let target_idx = *target_path.last().unwrap();
        let parent_key = if parent_path.is_empty() {
            self.root
        } else {
            self.get_node_key_at_path(parent_path)
        };
        let Some(parent_key) = parent_key else {
            self.append_leaf(tile, focus);
            return true;
        };

        let parent = match self.get_container(parent_key) {
            Some(container) => container,
            None => {
                self.append_leaf(tile, focus);
                return true;
            }
        };
        let target_key = match parent.child_key(target_idx) {
            Some(key) => key,
            None => {
                self.append_leaf(tile, focus);
                return true;
            }
        };

        let parent_layout = parent.layout();
        if matches!(parent_layout, Layout::SplitH | Layout::SplitV)
            && parent_layout == desired_layout
        {
            // The parent already splits along this axis: insert as a plain sibling.
            let insert_idx = if direction.is_leading() {
                target_idx
            } else {
                target_idx + 1
            };
            let tile_key = self.insert_node(NodeData::Leaf(tile));

            let container = self
                .get_container_mut(parent_key)
                .expect("insert split parent missing");
            container.insert_child(insert_idx, tile_key);

            self.set_parent(tile_key, Some(parent_key));
            self.settle_focus_after_insert(tile_key, focus);
            return true;
        }

        // Otherwise wrap the target and the new tile in a fresh split container that
        // replaces the target in its parent.
        let tile_key = self.insert_node(NodeData::Leaf(tile));
        let new_container_key = self.new_split_pair_container(target_key, tile_key, direction);

        let container = self
            .get_container_mut(parent_key)
            .expect("insert split parent missing");
        container.replace_child_preserving_focus(target_key, new_container_key);
        self.set_parent(new_container_key, Some(parent_key));

        self.settle_focus_after_insert(tile_key, focus);

        true
    }

    pub(in crate::layout) fn insert_leaf_split_root(
        &mut self,
        direction: Direction,
        tile: Tile<W>,
        focus: bool,
    ) -> bool {
        let Some(root_key) = self.root else {
            self.append_leaf(tile, focus);
            return true;
        };

        let tile_key = self.insert_node(NodeData::Leaf(tile));

        if let Some(root_container) = self.get_container(root_key) {
            if root_container.layout() == direction.split_layout() {
                // The root already splits along this axis: insert at the edge.
                let insert_idx = if direction.is_leading() {
                    0
                } else {
                    root_container.child_count()
                };
                if let Some(container) = self.get_container_mut(root_key) {
                    container.insert_child(insert_idx, tile_key);
                }

                self.set_parent(tile_key, Some(root_key));
                self.settle_focus_after_insert(tile_key, focus);
                return true;
            }
        }

        let new_container_key = self.new_split_pair_container(root_key, tile_key, direction);
        self.set_parent(new_container_key, None);
        self.root = Some(new_container_key);

        self.settle_focus_after_insert(tile_key, focus);

        true
    }
}
