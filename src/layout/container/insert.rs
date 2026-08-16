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
        let root = self.root;
        self.insert_window_into_branch(root, tile, focus);
    }

    /// The same, into one branch: the tiled side, or one floating group.
    pub(in crate::layout) fn insert_window_into_branch(
        &mut self,
        branch_root: NodeKey,
        tile: Tile<W>,
        focus: bool,
    ) {
        self.clear_focus_history();

        let tile_key = self.insert_node(NodeData::Leaf(tile));
        self.insert_key_as_focus_sibling(branch_root, tile_key, focus);
    }

    /// Insert a detached subtree into the tree, optionally focusing it afterwards.
    pub(in crate::layout) fn insert_subtree_with_focus(
        &mut self,
        subtree: DetachedNode<W>,
        focus: bool,
    ) {
        self.clear_focus_history();

        let node_key = self.insert_subtree(subtree);
        let root = self.root;
        self.insert_key_as_focus_sibling(root, node_key, focus);
    }

    /// Put a new node where sway puts a new window: next to whatever was most recently
    /// focused under the workspace, which is `seat_get_focus_inactive` there.
    ///
    /// `focus parent` puts a container at the head of that history, so selecting one moves
    /// where a window lands — it joins the container's *siblings* rather than going inside.
    /// The workspace is the one node that is never its own answer, because it is not under
    /// itself: selecting it hands the question down to its focused child, and a window
    /// opened there lands beside that child rather than at the end of the row.
    pub(super) fn insert_key_as_focus_sibling(
        &mut self,
        branch_root: NodeKey,
        node_key: NodeKey,
        focus: bool,
    ) {
        let sibling_key = match self
            .selected_key()
            .filter(|key| self.get_node(*key).is_some() && self.branch_root(*key) == branch_root)
        {
            Some(key) if key != branch_root => Some(key),
            Some(_) => self.active_child(branch_root),
            None => self
                .effective_focused_key()
                .filter(|key| self.branch_root(*key) == branch_root)
                .or_else(|| {
                    // sway's `view_map` takes this exact two-step route when the active node
                    // is floating: choose the most recent tiling node first, then the most
                    // recent view inside that node. Going straight to a view under the whole
                    // workspace loses a recently active container as the insertion context.
                    self.focus_inactive_node_in_branch(branch_root)
                        .and_then(|node| {
                            if node == branch_root {
                                self.focus_inactive_view_in_branch(branch_root)
                            } else {
                                self.focus_inactive_view(node)
                            }
                        })
                }),
        };

        let insert_target = sibling_key
            .and_then(|key| {
                let parent_key = self.parent_of(key)?;
                Some((parent_key, self.child_index(parent_key, key)? + 1))
            })
            // An empty branch has nothing to sit beside.
            .or_else(|| Some((branch_root, self.get_container(branch_root)?.child_count())));

        let Some((parent_key, insert_idx)) = insert_target else {
            return;
        };
        if let Some(parent_container) = self.get_container_mut(parent_key) {
            parent_container.insert_child(insert_idx, node_key);
            self.set_parent(node_key, Some(parent_key));
            self.settle_focus_after_insert(node_key, focus);
        }
    }

    /// Insert a leaf as the next sibling of `window_id`, or at the end of the workspace when
    /// that window is not on the tiled side.
    ///
    /// The slot is resolved before the tile is consumed. A tile that has been moved into the
    /// arena and then not attached is a window the client believes is mapped and the tree
    /// cannot show, so every route here has to end in an insertion.
    pub(in crate::layout) fn insert_leaf_after(
        &mut self,
        window_id: &W::Id,
        tile: Tile<W>,
        focus: bool,
    ) {
        let slot = self
            .window_key(window_id)
            .filter(|key| self.branch_root(*key) == self.root)
            .and_then(|current_key| {
                let parent_key = self.parent_of(current_key)?;
                let current_idx = self.child_index(parent_key, current_key)?;
                Some((parent_key, current_idx + 1))
            });

        let Some((parent_key, insert_idx)) = slot else {
            self.append_leaf(tile, focus);
            return;
        };

        let tile_key = self.insert_node(NodeData::Leaf(tile));
        self.get_container_mut(parent_key)
            .expect("child_index resolved this parent as a layout parent")
            .insert_child(insert_idx, tile_key);
        self.set_parent(tile_key, Some(parent_key));
        self.settle_focus_after_insert(tile_key, focus);
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

    /// Where a window sits, addressed within its own branch, so it can be put back there.
    pub(in crate::layout) fn insert_parent_info_for_window(
        &self,
        window_id: &W::Id,
    ) -> Option<InsertParentInfo> {
        let key = self.window_key(window_id)?;
        self.insert_parent_info_for_node(key)
    }

    /// Where a node sits in the tiled branch before `container_set_floating` detaches it.
    pub(in crate::layout) fn insert_parent_info_for_node(
        &self,
        key: NodeKey,
    ) -> Option<InsertParentInfo> {
        let branch_root = self.branch_root(key);
        let path = self.branch_relative_path(key)?;
        self.insert_parent_info_for_path(branch_root, &path)
    }

    pub(super) fn insert_parent_info_for_path(
        &self,
        branch_root: NodeKey,
        path: &[usize],
    ) -> Option<InsertParentInfo> {
        if path.is_empty() {
            return None;
        }

        let mut parent_path = path.to_vec();
        let insert_idx = parent_path.pop().unwrap();
        let parent_key = if parent_path.is_empty() {
            branch_root
        } else {
            self.node_at_branch_path(branch_root, &parent_path)?
        };
        let parent = self.get_container(parent_key)?;
        Some(InsertParentInfo {
            parent_path,
            insert_idx,
            layout: parent.layout(),
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
        branch_root: NodeKey,
        info: &InsertParentInfo,
        tile: Tile<W>,
        focus: bool,
    ) -> bool {
        let tile_key = self.insert_node(NodeData::Leaf(tile));
        self.insert_key_with_parent_info(branch_root, info, tile_key, focus)
    }

    #[cfg(test)]
    pub(in crate::layout) fn insert_subtree_with_parent_info(
        &mut self,
        info: &InsertParentInfo,
        subtree: DetachedNode<W>,
        focus: bool,
    ) -> bool {
        let node_key = self.insert_subtree(subtree);
        let root = self.root;
        self.insert_key_with_parent_info(root, info, node_key, focus)
    }

    /// Insert an already-materialized node at the container described by `info`.
    fn insert_key_with_parent_info(
        &mut self,
        branch_root: NodeKey,
        info: &InsertParentInfo,
        node_key: NodeKey,
        focus: bool,
    ) -> bool {
        // A path recorded elsewhere can resolve here to a node that holds a window rather than
        // children, and a window has no room for one. Treat that the same as finding no
        // container at all: the node goes at the end of the branch, which is where the
        // remembered position has stopped meaning anything.
        let container_key = self
            .ensure_container_at_path(branch_root, &info.parent_path, info.layout)
            .filter(|key| self.get_container(*key).is_some());

        let Some(container_key) = container_key else {
            let end = self.branch_children_len(branch_root);
            self.insert_key_into_branch(branch_root, end, node_key, focus);
            return true;
        };

        self.get_container_mut(container_key)
            .expect("only a node that lays out children gets here")
            .insert_child(info.insert_idx, node_key);
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
