//! Window and subtree insertion at focus, path or split targets.

use super::ContainerArena;
use super::ContainerData;
use super::DetachedNode;
use super::Direction;
use super::InsertParentInfo;
use super::Layout;
use super::LayoutElement;
use super::NodeData;
use super::NodeKey;
use crate::layout::tile::Tile;

impl<W: LayoutElement> ContainerArena<W> {
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

        // Before the tile exists, so the split never has to reason about an arena node that
        // is not attached to anything yet.
        self.autotile_presplit(branch_root);

        let tile_key = self.insert_node(NodeData::Container(ContainerData::new_view(tile)));
        self.insert_key_as_focus_sibling(branch_root, tile_key, focus);
    }

    /// Autotiling: split the node a new window is about to land beside, against its shape.
    ///
    /// This is the `autotiling` script's whole trick, moved inside the compositor. The script
    /// watches for a window about to map and issues `split h` or `split v` on the focused
    /// container first, so the wrapper sway then builds runs along the node's long axis. Doing
    /// it here rather than over IPC means the split and the insertion cannot be separated by
    /// another command, and that the window never maps into the pre-split shape and resizes.
    ///
    /// The split is the same [`Self::split_target`] the keybinding calls, so a wrapper it
    /// builds is a user container exactly as the script's would be: the mode chooses the
    /// orientation, it does not invent a different kind of container.
    ///
    /// The rule the whole mode follows is that it decides only what nobody decided. It splits
    /// wherever the tree grows on its own — a window mapping, a directional move landing
    /// beside something — and stands down wherever a command placed the node: `focus parent`,
    /// a switcher, an explicit `split`. Which is why the promise it makes is not "the tree is
    /// binary". It is narrower and it is testable: open, close and move windows all you like
    /// and no split container below the workspace ever holds three. Ask for a five-way row
    /// and you get a five-way row.
    fn autotile_presplit(&mut self, branch_root: NodeKey) {
        // Floating groups have no row to dwindle into; their nodes carry their own boxes.
        if branch_root != self.root {
            return;
        }

        let Some(target) = self
            .view_map_target()
            .filter(|key| self.branch_root(*key) == branch_root)
        else {
            // An empty workspace: the first window is a plain child of the workspace, and the
            // orientation only becomes a question once there is something to sit beside.
            return;
        };

        // `focus parent` aims the insertion at a container's siblings rather than at a window.
        // Splitting there would state an orientation for a subtree the user selected on
        // purpose, so the mode stands down and the plain sway placement applies.
        if !self.get_node(target).is_some_and(|node| node.is_view()) {
            return;
        }

        let Some(layout) = self.autotile_layout_for(target) else {
            return;
        };
        self.split_target(layout, target);
    }

    /// The orientation autotiling gives a node, read off the box that node is holding.
    ///
    /// `None` whenever the mode has nothing to say: it is off, the node is not on the tiled
    /// side, its parent is a switcher — where "beside" already means another tab — or it has
    /// no box to measure yet.
    pub(super) fn autotile_layout_for(&self, key: NodeKey) -> Option<Layout> {
        if !self.options.layout.autotile {
            return None;
        }
        if self.branch_root(key) != self.root {
            return None;
        }
        if self.parent_is_switcher(key) {
            return None;
        }

        let rect = self.node_geometry(key)?;
        if rect.size.w <= 0. || rect.size.h <= 0. {
            return None;
        }

        let ratio = self.options.layout.autotile_ratio;
        Some(if rect.size.w >= rect.size.h * ratio {
            Layout::SplitH
        } else {
            Layout::SplitV
        })
    }

    /// Autotiling for an arriving node that is not a new window: where a *move* should put it.
    ///
    /// A directional move reparents into an existing list, so a node landing beside a pair
    /// makes it a trio — the arity nobody asked for. The mode answers the same way it answers
    /// at map time: split the node being landed beside, and let the arrival pair with it
    /// inside the wrapper that produces. Returns the parent and index the caller should
    /// reparent into, or `None` to leave sway's own placement alone.
    ///
    /// `after` is whether the arriving node belongs on the far side of `neighbour`.
    pub(super) fn autotile_pair_slot(
        &mut self,
        neighbour: NodeKey,
        after: bool,
    ) -> Option<(NodeKey, usize)> {
        let parent_key = self.parent_of(neighbour)?;

        // A list that is about to reach two is exactly what the mode wants; there is nothing
        // to correct, and wrapping here would only add a level sway would not have.
        if self.get_container(parent_key)?.child_count() < 2 {
            return None;
        }

        let layout = self.autotile_layout_for(neighbour)?;
        // `split` keeps the command context on what it split, which during a move would leave
        // the selection on a container the user never selected. The move settles focus itself
        // afterwards; the selection has to come back here.
        let selected_before = self.selected_key();
        if !self.split_target(layout, neighbour) {
            return None;
        }
        if self.selected_key() != selected_before {
            self.seat.redirect_selection(selected_before);
        }

        // The split builds a wrapper only when it has something to separate the node from.
        // Where it settled for restating an orientation instead, there is no new slot.
        let wrapper_key = self.parent_of(neighbour)?;
        if wrapper_key == parent_key {
            return None;
        }
        Some((wrapper_key, usize::from(after)))
    }

    /// Autotiling hygiene: dissolve the containers a departure left holding a single child.
    ///
    /// sway leaves them standing — `container_reap_empty` destroys a container with *no*
    /// children, and the squash that would flatten this one runs only after a directional
    /// move and only where two levels say the same thing. So a close inside a pair leaves
    /// `SplitV[B]` wrapping one window forever, and a dwindle layout accumulates those levels
    /// until nothing lines up with anything. Under the mode the level goes, which is
    /// `split none` on what is left.
    ///
    /// This sweeps the branch rather than one key on purpose. A move can reparent the node,
    /// reorient the workspace and leave a wrapper behind in one command, so the container
    /// that ends up holding a single child is not always the one the caller was holding a key
    /// to when it started.
    pub(super) fn autotile_squash_lone_children(&mut self) {
        if !self.options.layout.autotile {
            return;
        }

        // `split none` climbs through single-child ancestors, so each pass can dissolve a
        // whole chain. The bound is the node count: no pass that finds work leaves the tree
        // with more nodes than it started with.
        for _ in 0..self.nodes.len() {
            let Some(child_key) = self.lone_split_child(self.root) else {
                return;
            };
            if self.unsplit_target(child_key).is_none() {
                return;
            }
        }
    }

    /// The child of the first container below the workspace that is holding it alone.
    fn lone_split_child(&self, key: NodeKey) -> Option<NodeKey> {
        let container = self.get_container(key)?;
        let children = container.children.clone();
        // A view holds a tile, not an arrangement; only a real container can be a level.
        let is_real_container = self.get_real_container(key).is_some();

        // The workspace holding a single container is the ordinary shape of a split tree, and
        // `split none` is defined never to climb through it. A one-tab switcher is a switcher
        // the user asked for.
        if key != self.root
            && is_real_container
            && children.len() == 1
            && matches!(
                self.get_container(key)?.layout(),
                Layout::SplitH | Layout::SplitV
            )
        {
            return Some(children[0]);
        }

        children
            .into_iter()
            .find_map(|child_key| self.lone_split_child(child_key))
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
        let sibling_key = self
            .view_map_target()
            .filter(|key| self.branch_root(*key) == branch_root);

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

    /// The exact existing node beside which Sway maps a normal new view.
    ///
    /// `seat_get_focus_inactive(workspace)` is a read of the global focus stack, not of the
    /// selected workspace's active tiled child. That distinction matters after focusing the
    /// workspace from floating: the floating root remains first behind the workspace in the
    /// stack. A top-level floating node is not an insertion parent, so `view_map` falls back
    /// through the most recent tiling node and then its inactive view. A descendant inside a
    /// floating split *is* a normal insertion target.
    ///
    /// sway/tree/view.c:802-824
    pub(in crate::layout) fn view_map_target(&self) -> Option<NodeKey> {
        let inactive = self.seat.order().iter().copied().find(|key| {
            *key != self.root
                && self.get_node(*key).is_some()
                && self.is_descendant(*key, self.root)
        })?;

        if self.is_floating_root(inactive) {
            self.focus_inactive_node_in_branch(self.root)
                .and_then(|node| self.focus_inactive_view(node))
        } else {
            Some(inactive)
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

        let tile_key = self.insert_node(NodeData::Container(ContainerData::new_view(tile)));
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

        if self
            .get_node(root_child_key)
            .is_some_and(|node| node.is_view())
        {
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

        let tile_key = self.insert_node(NodeData::Container(ContainerData::new_view(tile)));

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
        let existing = self.get_node_mut(key)?.as_tile_mut()?;
        Some(std::mem::replace(existing, tile))
    }

    /// Whether `key` addresses a leaf (window) node.
    pub(in crate::layout) fn is_leaf(&self, key: NodeKey) -> bool {
        self.get_node(key).is_some_and(|node| node.is_view())
    }

    pub(in crate::layout) fn insert_leaf_with_parent_info(
        &mut self,
        branch_root: NodeKey,
        info: &InsertParentInfo,
        tile: Tile<W>,
        focus: bool,
    ) -> bool {
        let tile_key = self.insert_node(NodeData::Container(ContainerData::new_view(tile)));
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
            let tile_key = self.insert_node(NodeData::Container(ContainerData::new_view(tile)));

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
        let tile_key = self.insert_node(NodeData::Container(ContainerData::new_view(tile)));
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

        let tile_key = self.insert_node(NodeData::Container(ContainerData::new_view(tile)));
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
