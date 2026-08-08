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
        self.insert_key_as_focus_sibling(node_key, focus);
    }

    /// Make `subtree` the whole tree, replacing whatever root is there.
    ///
    /// For a floating container, where the tree's root *is* the container the user sees: a
    /// grouped subtree arriving is that container, not something to put inside one. Inserting
    /// it under the root instead would add a level the floating side has to see through, and
    /// `focus parent` counts levels.
    pub(in crate::layout) fn adopt_subtree_as_root(
        &mut self,
        subtree: DetachedNode<W>,
        focus: bool,
    ) {
        self.clear_focus_history();

        let old_root = self.root;
        let node_key = self.insert_subtree(subtree);
        if matches!(self.get_node(node_key), Some(NodeData::Container(_))) {
            self.remove_node_recursive(old_root);
            self.set_parent(node_key, None);
            self.root = node_key;
            self.focused_key = None;
            self.selected_key = None;
            self.focus_first_leaf();
            if !focus {
                self.focused_key = self.first_leaf_key();
            }
            return;
        }

        // A lone window still needs the container to live in.
        self.insert_key_as_focus_sibling(node_key, focus);
    }

    /// Put a new node where sway puts a new window: next to whatever was most recently
    /// focused under the workspace, which is `seat_get_focus_inactive` there.
    ///
    /// `focus parent` puts a container at the head of that history, so selecting one moves
    /// where a window lands — it joins the container's *siblings* rather than going inside.
    /// The workspace is the one node that is never its own answer, because it is not under
    /// itself: selecting it hands the question down to its focused child, and a window
    /// opened there lands beside that child rather than at the end of the row.
    pub(super) fn insert_key_as_focus_sibling(&mut self, node_key: NodeKey, focus: bool) {
        let sibling_key = match self
            .selected_key
            .filter(|key| self.get_node(*key).is_some())
        {
            Some(key) if key != self.root => Some(key),
            Some(_) => self
                .get_container(self.root)
                .and_then(ContainerData::focused_child_key),
            None => self.effective_focused_key(),
        };

        let insert_target = sibling_key
            .and_then(|key| {
                let parent_key = self.parent_of(key)?;
                Some((parent_key, self.child_index(parent_key, key)? + 1))
            })
            // An empty workspace has nothing to sit beside.
            .or_else(|| Some((self.root, self.get_container(self.root)?.child_count())));

        let Some((parent_key, insert_idx)) = insert_target else {
            return;
        };
        if let Some(NodeData::Container(parent_container)) = self.get_node_mut(parent_key) {
            parent_container.insert_child(insert_idx, node_key);
            self.set_parent(node_key, Some(parent_key));
            self.settle_focus_after_insert(node_key, focus);
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

        let parent_key = self.get_node_key_at_path(parent_path);

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
        let root_key = self.root;

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
            let Some(_) = self.wrap_child_in_new_container(
                root_key,
                root_child_key,
                ContainerData::new(Layout::SplitV),
            ) else {
                return false;
            };
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
            self.root
        } else {
            self.get_node_key_at_path(&parent_path)?
        };
        let parent = self.get_container(parent_key)?;
        Some(InsertParentInfo {
            parent_path,
            insert_idx,
            layout: parent.layout(),
            fractions: Some(parent.fractions.clone()),
        })
    }

    /// Swap the tile held by the leaf at `key`, returning the previous one.
    pub(in crate::layout) fn replace_leaf(
        &mut self,
        key: NodeKey,
        tile: Tile<W>,
    ) -> Option<Tile<W>> {
        match self.get_node_mut(key)? {
            NodeData::Leaf(existing) => Some(std::mem::replace(existing, tile)),
            _ => None,
        }
    }

    /// Whether `key` addresses a leaf (window) node.
    pub(in crate::layout) fn is_leaf(&self, key: NodeKey) -> bool {
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
            if let Some(fractions) = info.fractions.as_ref().filter(|fractions| {
                container.layout() == info.layout
                    && fractions.is_compatible_with(container.child_count())
            }) {
                // This is an exact snapshot, including any fractions Sway has deliberately
                // left unset under a tabbed/stacked layout. Resolution belongs to arrange,
                // not restoration.
                container.fractions = fractions.clone();
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
        container.mark_user_created();
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
        target_key: NodeKey,
        direction: Direction,
        tile: Tile<W>,
        focus: bool,
    ) -> bool {
        if self.is_empty() {
            self.append_leaf(tile, focus);
            return true;
        }

        let desired_layout = direction.split_layout();

        // The workspace has no sibling slot to split into, so the window simply joins it.
        let Some(parent_key) = self.parent_of(target_key) else {
            self.append_leaf(tile, focus);
            return true;
        };

        let Some(target_idx) = self.child_index(parent_key, target_key) else {
            self.append_leaf(tile, focus);
            return true;
        };
        let Some(parent) = self.get_container(parent_key) else {
            self.append_leaf(tile, focus);
            return true;
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

        self.replace_child_node(parent_key, target_key, new_container_key);

        self.settle_focus_after_insert(tile_key, focus);

        true
    }

    pub(in crate::layout) fn insert_leaf_split_root(
        &mut self,
        direction: Direction,
        tile: Tile<W>,
        focus: bool,
    ) -> bool {
        let root_key = self.root;

        // The workspace has to face the direction for an edge to mean anything. When it does
        // not, its children move under one container and it turns — the same surgery a move
        // across the workspace does, and the reason it is one call rather than a rule here.
        if self.root_container_layout() != direction.split_layout() && !self.is_empty() {
            let previous = self.root_container_layout();
            self.wrap_workspace_children(previous, direction.split_layout());
        }

        let tile_key = self.insert_node(NodeData::Leaf(tile));
        let insert_idx = match direction.is_leading() {
            true => 0,
            false => self
                .get_container(root_key)
                .map_or(0, |root| root.child_count()),
        };
        if let Some(container) = self.get_container_mut(root_key) {
            container.insert_child(insert_idx, tile_key);
        }
        self.set_parent(tile_key, Some(root_key));

        self.settle_focus_after_insert(tile_key, focus);

        true
    }
}
