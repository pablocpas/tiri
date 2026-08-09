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
        let node_key = self.window_key(window_id)?;
        let cleanup_key = self.parent_of(node_key);
        let was_focused = self.focused_key() == Some(node_key);

        // Detach from the parent's child list before dropping the node itself. The
        // workspace has no parent and is not a window, so nothing routes here for it.
        let parent_key = self.parent_of(node_key)?;
        if let Some(child_idx) = self.child_index(parent_key, node_key) {
            if let Some(container) = self.get_container_mut(parent_key) {
                container.remove_child(child_idx);
            }
        }
        self.set_parent(node_key, None);

        // Now remove from slotmap (only the leaf, not recursive)
        let node_data = self.nodes.remove(node_key)?;
        self.parents.remove(node_key);
        let tile = match node_data {
            NodeData::Leaf(tile) => tile,
            NodeData::Container(_) => return None, // Should never happen
        };

        if let Some(cleanup_key) = cleanup_key {
            self.reap_empty(cleanup_key);
        }
        self.prune_leaf_layouts();

        self.prune_selected_key();
        self.reconcile_focus_after_change(was_focused);

        self.layout();

        Some(tile)
    }

    /// Detach the subtree rooted at `node_key`, returning it along with enough information
    /// to put it back where it was.
    pub(in crate::layout) fn take_subtree_at(
        &mut self,
        node_key: NodeKey,
    ) -> Option<(DetachedNode<W>, Option<InsertParentInfo>)> {
        self.get_node(node_key)?;
        let insert_info = self
            .find_node_path(node_key)
            .and_then(|path| self.insert_parent_info_for_path(&path));

        let focused_in_subtree = self
            .focused_key()
            .is_some_and(|key| self.is_descendant_of(key, node_key));

        if let Some(selected_key) = self.selected_key() {
            if self.is_descendant_of(selected_key, node_key) {
                self.seat.redirect_selection(None);
            }
        }

        // Taking the workspace itself is not one of these operations.
        let parent_key = Some(self.parent_of(node_key)?);
        let cleanup_key = match parent_key {
            None => None,
            Some(parent_key) => {
                if let Some(idx) = self.child_index(parent_key, node_key) {
                    if let Some(container) = self.get_container_mut(parent_key) {
                        container.remove_child(idx);
                    }
                }
                self.set_parent(node_key, None);
                Some(parent_key)
            }
        };

        let subtree = self.extract_subtree(node_key);
        if let Some(cleanup_key) = cleanup_key {
            self.reap_empty(cleanup_key);
        }
        self.prune_leaf_layouts();

        self.prune_selected_key();
        self.reconcile_focus_after_change(focused_in_subtree);

        self.layout();

        Some((subtree, insert_info))
    }

    /// Everything in the tree as one subtree, leaving an empty workspace behind.
    ///
    /// The detached container carries the workspace's own layout, so putting it back rebuilds
    /// exactly what was taken. Floating the whole workspace used to wrap its children in a
    /// container first, which is a node the tiling side then had to recognise and remove on
    /// the way back; taking the workspace's contents directly leaves nothing to undo.
    ///
    /// A fresh root replaces the one that left, because the workspace outlives its contents.
    pub(in crate::layout) fn take_whole_tree(&mut self) -> Option<DetachedNode<W>> {
        self.first_leaf_key()?;
        let old_root = self.root;
        let layout = self.root_container_layout();
        self.seat.clear();
        let subtree = self.extract_subtree(old_root);
        self.root = self.insert_node(NodeData::Container(ContainerData::new(layout)));
        self.set_parent(self.root, None);
        self.leaf_layouts.clear();
        Some(subtree)
    }

    /// The inverse of [`Self::take_whole_tree`]: the subtree becomes the workspace again.
    ///
    /// Its layout and children replace the empty root's rather than being hung under it,
    /// which is what makes the round trip give back exactly the tree that left.
    pub(in crate::layout) fn restore_whole_tree(&mut self, subtree: DetachedNode<W>, focus: bool) {
        self.clear_focus_history();

        let node_key = self.insert_subtree(subtree);
        let Some(container) = self.get_container(node_key) else {
            // A lone window: it needs the workspace to live in, like any other.
            self.insert_key_as_focus_sibling(node_key, focus);
            return;
        };

        let layout = container.layout();
        let children = container.children.clone();
        let fractions = container.fractions.clone();
        let prev_split_layout = container.prev_split_layout;

        let root_key = self.root;
        if let Some(root) = self.get_container_mut(root_key) {
            root.set_layout(layout);
            root.children = children.clone();
            root.fractions = fractions;
            root.prev_split_layout = prev_split_layout;
        }
        for child in children {
            self.set_parent(child, Some(root_key));
        }
        self.nodes.remove(node_key);
        self.parents.remove(node_key);

        self.focus_first_leaf();
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
                // The order has to travel as positions: the keys do not survive leaving the
                // tree, and a subtree that comes back has new ones. Which child a switcher
                // shows is behaviour, so it cannot be dropped and re-derived on arrival.
                let mut focus_stack: Vec<usize> = self
                    .seat
                    .order()
                    .iter()
                    .filter_map(|key| index_by_key.get(key).copied())
                    .collect();
                for idx in 0..child_keys.len() {
                    if !focus_stack.contains(&idx) {
                        focus_stack.push(idx);
                    }
                }
                DetachedNode::Container(DetachedContainer {
                    layout: container.layout,
                    children,
                    fractions: container.fractions,
                    focus_stack,
                    user_created: container.user_created,
                    prev_split_layout: container.prev_split_layout,
                })
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
                    node.fractions = container.fractions;
                    node.user_created = container.user_created;
                    node.prev_split_layout = container.prev_split_layout;
                    if !node.fractions.is_compatible_with(node.children.len()) {
                        node.fractions.resize_unset(node.children.len());
                        node.recalculate_percentages();
                    }
                }

                // Back into the seat's order, keeping the sequence the subtree carried.
                // Appended rather than promoted: arriving is not being focused, and whatever
                // focuses next will raise its own chain.
                let restored: Vec<NodeKey> = container
                    .focus_stack
                    .iter()
                    .filter_map(|idx| {
                        self.get_container(container_key)?
                            .children()
                            .get(*idx)
                            .copied()
                    })
                    .collect();
                // Appended, not placed: a subtree arriving from another tree has no standing
                // to restore. It left one arena and came back to another, so the keys it had
                // are gone and with them its place in the order. See `docs/design/parity.md`
                // on the two trees.
                self.seat.restore_at(usize::MAX, restored);

                container_key
            }
        }
    }

    /// Remove and return the root-level child at the given index as a detached subtree.
    pub(in crate::layout) fn take_root_child_subtree(
        &mut self,
        idx: usize,
    ) -> Option<DetachedNode<W>> {
        let root_key = self.root;

        match self.get_node(root_key) {
            Some(NodeData::Leaf(_)) => None,
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
