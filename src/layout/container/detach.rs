//! Detaching and reattaching subtrees (window removal, float/unfloat surgery).

use super::ContainerArena;
use super::ContainerData;
use super::DetachedNode;
#[cfg(test)]
use super::InsertParentInfo;
use super::LayoutElement;
use super::NodeData;
use super::NodeKey;
use crate::layout::tile::Tile;

impl<W: LayoutElement> ContainerArena<W> {
    /// Remove a window by ID, returns the removed tile
    pub(in crate::layout) fn remove_window(&mut self, window_id: &W::Id) -> Option<Tile<W>> {
        let node_key = self.window_key(window_id)?;
        let cleanup_key = self.parent_of(node_key);
        let was_focused = self.focused_key() == Some(node_key);
        let removed_from_fullscreen = self
            .fullscreen_key
            .is_some_and(|scope| node_key == scope || self.is_descendant(node_key, scope));
        let former_ancestors = cleanup_key
            .map(|parent| self.focus_chain(parent))
            .unwrap_or_default();

        // Detach from the parent's child list before dropping the node itself. The
        // workspace has no parent and is not a window, so nothing routes here for it.
        let parent_key = self.parent_of(node_key)?;
        if let Some(child_idx) = self.child_index(parent_key, node_key) {
            if let Some(container) = self.get_container_mut(parent_key) {
                container.remove_child(child_idx);
            }
        }
        self.set_parent(node_key, None);

        // Now remove from this workspace store (only the leaf, not recursive).
        let node_data = self.remove_node_from_store(node_key)?;
        let tile = node_data.into_tile()?;

        if let Some(cleanup_key) = cleanup_key {
            self.reap_empty(cleanup_key);
        }
        self.prune_leaf_layouts();

        self.prune_selected_key();
        self.reconcile_focus_after_change(was_focused, &former_ancestors, removed_from_fullscreen);

        self.layout();

        Some(tile)
    }

    /// Detach the subtree rooted at `node_key`, returning it along with enough information
    /// to put it back where it was.
    #[cfg(test)]
    pub(in crate::layout) fn take_subtree_at(
        &mut self,
        node_key: NodeKey,
    ) -> Option<(DetachedNode<W>, Option<InsertParentInfo>)> {
        self.get_node(node_key)?;
        let branch_root = self.branch_root(node_key);
        let insert_info = self
            .branch_relative_path(node_key)
            .and_then(|path| self.insert_parent_info_for_path(branch_root, &path));

        let subtree = self.take_tiling_subtree(node_key)?;
        Some((subtree, insert_info))
    }

    /// Detach the command target for `move container`, preserving the addressed subtree.
    ///
    /// With no selected parent this is the focused leaf, including under tabbed/stacked. A
    /// parent selected through `focus parent` is intentionally moved as a group. The workspace
    /// itself is not a container target and therefore cannot be detached here.
    pub(in crate::layout) fn take_command_target_subtree(&mut self) -> Option<DetachedNode<W>> {
        let key = self.command_target_in(self.root);
        if key == self.root {
            return None;
        }
        self.take_tiling_subtree(key)
    }

    fn take_tiling_subtree(&mut self, node_key: NodeKey) -> Option<DetachedNode<W>> {
        self.get_node(node_key)?;
        if node_key == self.root || self.branch_root(node_key) != self.root {
            return None;
        }

        // Any outstanding layout describes a branch that still contains the node being
        // transferred. It cannot govern either the source after detachment or the destination.
        self.discard_layout_superseded_by_transfer();

        let focused_in_subtree = self
            .focused_key()
            .is_some_and(|key| self.is_descendant_of(key, node_key));
        let former_ancestors = self
            .parent_of(node_key)
            .map(|parent| self.focus_chain(parent))
            .unwrap_or_default();

        if let Some(selected_key) = self.selected_key() {
            if self.is_descendant_of(selected_key, node_key) {
                self.seat.redirect_selection(None);
            }
        }

        let parent_key = self.parent_of(node_key)?;
        if let Some(idx) = self.child_index(parent_key, node_key) {
            if let Some(container) = self.get_container_mut(parent_key) {
                container.remove_child(idx);
            }
        }
        self.set_parent(node_key, None);

        let subtree = self.extract_subtree(node_key);
        self.reap_empty(parent_key);
        self.prune_leaf_layouts();

        self.prune_selected_key();
        self.reconcile_focus_after_change(focused_in_subtree, &former_ancestors, false);

        self.layout();

        Some(subtree)
    }

    /// Extract a subtree rooted at the given key into a detached representation.
    pub(super) fn extract_subtree(&mut self, key: NodeKey) -> DetachedNode<W> {
        // A view has no children to walk and no focus stack to carry; everything else on the
        // node travels the same way either way, which is the point of it being one type.
        let child_keys = self
            .get_container(key)
            .map(|container| container.children.clone())
            .unwrap_or_default();

        // Which child a switcher shows is behaviour, so the subtree carries those stable
        // identities into the receiving workspace instead of rebuilding them from child
        // positions. Read before anything leaves the arena: leaving is what retires a key from
        // the seat, so afterwards there is no order left to read and every arriving subtree
        // would land in child order, showing whichever window happens to be first.
        let mut focus_stack: Vec<NodeKey> = self
            .seat
            .order()
            .iter()
            .copied()
            .filter(|key| child_keys.contains(key))
            .collect();
        for key in child_keys.iter().copied() {
            if !focus_stack.contains(&key) {
                focus_stack.push(key);
            }
        }

        let node_data = self
            .remove_node_from_store(key)
            .expect("node key must exist when extracting subtree");

        let NodeData::Container(container) = node_data else {
            unreachable!("the workspace cannot be detached")
        };

        let mut children = Vec::new();
        for child_key in child_keys {
            children.push(self.extract_subtree(child_key));
        }

        // `sizing` is the split's own; a view's lives on its tile, here as in the arena.
        let detached = DetachedNode {
            key,
            sizing: container.sizing,
            layout: container.layout,
            user_created: container.user_created,
            prev_split_layout: container.prev_split_layout,
            tile: container.into_tile(),
            children,
            focus_stack,
        };
        debug_assert!(detached
            .tile
            .as_ref()
            .is_none_or(|tile| tile.node_key() == key));
        detached
    }

    /// Insert a detached subtree into this tree, returning the new root key.
    pub(super) fn insert_subtree(&mut self, subtree: DetachedNode<W>) -> NodeKey {
        // A view keeps the key it left with, the same as a split: node identity surviving the
        // crossing is what the whole detached representation exists for.
        let container_key = subtree.key;
        let node = match subtree.tile {
            Some(tile) => ContainerData::new_view(tile),
            None => ContainerData::new(subtree.layout),
        };
        self.insert_node_with_key(container_key, NodeData::Container(node));

        let mut child_keys = Vec::new();
        for child in subtree.children {
            let child_key = self.insert_subtree(child);
            self.set_parent(child_key, Some(container_key));
            child_keys.push(child_key);
        }

        if let Some(node) = self.get_real_container_mut(container_key) {
            node.children = child_keys;
            node.sizing = subtree.sizing;
            node.user_created = subtree.user_created;
            node.prev_split_layout = subtree.prev_split_layout;
        }

        // Back into the seat's order, keeping the sequence the subtree carried.
        // Appended rather than promoted: arriving is not being focused, and whatever
        // focuses next will raise its own chain.
        let restored: Vec<NodeKey> = subtree
            .focus_stack
            .iter()
            .filter_map(|key| {
                self.get_container(container_key)?
                    .children()
                    .contains(key)
                    .then_some(*key)
            })
            .collect();
        // Appended, not placed: the receiving workspace has its own inactive order.
        // Layout's seat history decides whether the arriving view should be focused;
        // the order inside this subtree only decides which child a descent reaches.
        self.seat.restore_at(usize::MAX, restored);

        container_key
    }

    /// Remove and return the root-level child at the given index as a detached subtree.
    pub(in crate::layout) fn take_root_child_subtree(
        &mut self,
        idx: usize,
    ) -> Option<DetachedNode<W>> {
        let root_key = self.root;

        match self.get_node(root_key) {
            Some(node) if node.is_view() => None,
            Some(NodeData::Workspace(_)) => {
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

                self.prune_leaf_layouts();

                match self.get_node(root_key) {
                    Some(NodeData::Workspace(root_container)) => {
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
                    Some(node) if node.is_split() => {
                        unreachable!("root must be a workspace")
                    }
                    _ => self.focus_first_leaf(),
                }

                let subtree = self.extract_subtree(child_key);
                self.prune_selected_key();
                Some(subtree)
            }
            Some(NodeData::Container(_)) | None => None,
        }
    }
}
