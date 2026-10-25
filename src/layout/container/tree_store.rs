//! Tree node storage primitives: raw node access and parent links.

use smithay::utils::{Logical, Point, Rectangle};

use super::{
    ContainerData, ContainerTree, LayoutElement, LayoutMode, LayoutParentData, NewView, NodeData,
    NodeKey, WorkspaceData,
};
use crate::layout::tile::Tile;

impl<W: LayoutElement> ContainerTree<W> {
    /// Whether this tree still holds a node.
    pub(in crate::layout) fn holds_node(&self, key: NodeKey) -> bool {
        self.nodes.contains_key(key)
    }

    /// Get node data by key
    pub(super) fn get_node(&self, key: NodeKey) -> Option<&NodeData<W>> {
        self.nodes.get(key)
    }

    /// Get mutable node data by key
    pub(super) fn get_node_mut(&mut self, key: NodeKey) -> Option<&mut NodeData<W>> {
        self.nodes.get_mut(key)
    }

    /// Get the child-layout fields shared by the workspace and containers.
    pub(super) fn get_container(&self, key: NodeKey) -> Option<&LayoutParentData> {
        match self.nodes.get(key)? {
            NodeData::Workspace(workspace) => Some(workspace),
            NodeData::Container(container) if !container.is_view() => Some(container),
            NodeData::Container(_) => None,
        }
    }

    /// Get mutable child-layout fields shared by the workspace and containers.
    pub(super) fn get_container_mut(&mut self, key: NodeKey) -> Option<&mut LayoutParentData> {
        match self.nodes.get_mut(key)? {
            NodeData::Workspace(workspace) => Some(workspace),
            NodeData::Container(container) if !container.is_view() => Some(container),
            NodeData::Container(_) => None,
        }
    }

    /// Get a real container, excluding the workspace node.
    pub(super) fn get_real_container(&self, key: NodeKey) -> Option<&ContainerData<W>> {
        match self.nodes.get(key)? {
            NodeData::Container(container) if !container.is_view() => Some(container),
            _ => None,
        }
    }

    /// Any node that is not the workspace: a split container or a view alike.
    ///
    /// sway's `N_CONTAINER`, which is both. Use this for the state the two share — geometry,
    /// floating-root membership — and `get_real_container` when only a split will do.
    pub(super) fn get_any_container(&self, key: NodeKey) -> Option<&ContainerData<W>> {
        match self.nodes.get(key)? {
            NodeData::Container(container) => Some(container),
            NodeData::Workspace(_) => None,
        }
    }

    pub(super) fn get_any_container_mut(&mut self, key: NodeKey) -> Option<&mut ContainerData<W>> {
        match self.nodes.get_mut(key)? {
            NodeData::Container(container) => Some(container),
            NodeData::Workspace(_) => None,
        }
    }

    /// Get a mutable real container, excluding the workspace node.
    pub(super) fn get_real_container_mut(&mut self, key: NodeKey) -> Option<&mut ContainerData<W>> {
        match self.nodes.get_mut(key)? {
            NodeData::Container(container) if !container.is_view() => Some(container),
            _ => None,
        }
    }

    /// The workspace this tree lays out in.
    ///
    /// Always present: the root is a workspace node for the tree's whole life. The `Option`
    /// is for callers that would rather ask than assert.
    pub(super) fn get_workspace(&self, workspace: NodeKey) -> Option<&WorkspaceData> {
        match self.nodes.get(workspace)? {
            NodeData::Workspace(workspace) => Some(workspace),
            _ => None,
        }
    }

    pub(super) fn get_workspace_mut(&mut self, workspace: NodeKey) -> Option<&mut WorkspaceData> {
        match self.nodes.get_mut(workspace)? {
            NodeData::Workspace(workspace) => Some(workspace),
            _ => None,
        }
    }

    pub(super) fn set_parent(&mut self, child: NodeKey, parent: Option<NodeKey>) {
        assert!(
            self.nodes.set_parent(child, parent),
            "only a materialized node can be reparented"
        );
    }

    pub(in crate::layout) fn parent_of(&self, key: NodeKey) -> Option<NodeKey> {
        self.nodes.parent(key)
    }

    pub(super) fn child_index(&self, parent_key: NodeKey, child_key: NodeKey) -> Option<usize> {
        self.get_container(parent_key)?
            .children
            .iter()
            .position(|&key| key == child_key)
    }

    /// Replace a child, handing its parent share to the replacement.
    ///
    /// `container_replace` inserts the replacement beside the old node, unlinks the old one,
    /// then copies both fractions across. The resize totals do not move: they describe the
    /// rounded pending size of a particular node, while the fractions describe the slot the
    /// replacement has just taken.
    ///
    /// sway/tree/container.c:1534-1554
    pub(super) fn replace_child_node(
        &mut self,
        parent_key: NodeKey,
        old_key: NodeKey,
        new_key: NodeKey,
    ) -> bool {
        self.replace_child_node_with_fullscreen(parent_key, old_key, new_key, true)
    }

    fn replace_child_node_with_fullscreen(
        &mut self,
        parent_key: NodeKey,
        old_key: NodeKey,
        new_key: NodeKey,
        transfer_fullscreen: bool,
    ) -> bool {
        let Some(mut fractions) = self.node_fractions(old_key) else {
            return false;
        };
        // Despite storing both fractions as `double`, sway's `container_replace` saves them
        // in local `float` variables before handing the slot to the replacement. That
        // narrowing is observable at half-pixel boundaries after a resized node is wrapped:
        // the next arrange normalizes the slightly changed sibling total and can move the
        // rounded pixel to the last child.
        //
        // sway/tree/container.c:1548-1553
        fractions.width = fractions.width as f32 as f64;
        fractions.height = fractions.height as f32 as f64;
        let Some(parent) = self.get_container_mut(parent_key) else {
            return false;
        };
        let Some(idx) = parent.children.iter().position(|key| *key == old_key) else {
            return false;
        };
        parent.children[idx] = new_key;
        self.set_node_fractions(new_key, fractions);
        self.set_parent(new_key, Some(parent_key));
        if transfer_fullscreen {
            self.transfer_fullscreen_to_replacement(old_key, new_key);
        }
        true
    }

    /// Put a child under a new container without changing its parent slot.
    ///
    /// The wrapper takes over the child's box as well as its slot, which is sway's
    /// `container_split` copying `pending.x/y/width/height` off the child before replacing it.
    /// It is the same box either way once the next arrange runs; it is the answer in between,
    /// and while a fullscreen is up there is no next arrange to correct it.
    pub(in crate::layout) fn wrap_child_in_new_container(
        &mut self,
        parent_key: NodeKey,
        child_key: NodeKey,
        wrapper: ContainerData<W>,
    ) -> Option<NodeKey> {
        self.wrap_child_in_new_container_with_fullscreen(parent_key, child_key, wrapper, true)
    }

    fn wrap_child_in_new_container_with_fullscreen(
        &mut self,
        parent_key: NodeKey,
        child_key: NodeKey,
        mut wrapper: ContainerData<W>,
        transfer_fullscreen: bool,
    ) -> Option<NodeKey> {
        let workspace = self.workspace_of(parent_key)?;
        if wrapper.child_count() != 0 {
            return None;
        }
        let raw_focus_returns_to_child = self.seat.node() == Some(child_key);
        self.child_index(parent_key, child_key)?;
        wrapper.set_geometry(self.node_geometry(child_key)?);

        let wrapper_key = self.insert_node(workspace, NodeData::Container(wrapper));
        if !self.replace_child_node_with_fullscreen(
            parent_key,
            child_key,
            wrapper_key,
            transfer_fullscreen,
        ) {
            self.remove_node_recursive(wrapper_key);
            return None;
        }

        let wrapper = self
            .get_container_mut(wrapper_key)
            .expect("new wrapper container missing");
        wrapper.insert_child(0, child_key);
        self.set_parent(child_key, Some(wrapper_key));

        // `container_split` ends by putting the focus back, onto the wrapper and then onto the
        // child:
        //
        //     if (set_focus) {
        //         seat_set_raw_focus(seat, &cont->node);
        //         seat_set_raw_focus(seat, &child->node);
        //     }
        //
        // It reads like bookkeeping and it is the opposite: a container the seat has never
        // heard of answers for nothing, so without this the wrapper is invisible to
        // `seat_get_active_tiling_child` and its parent goes on showing a sibling.
        if raw_focus_returns_to_child {
            self.seat.raw_focus(wrapper_key);
            self.seat.raw_focus(child_key);
        }
        Some(wrapper_key)
    }

    /// The box a node is holding, whichever kind it is.
    pub(in crate::layout) fn node_geometry(&self, key: NodeKey) -> Option<Rectangle<f64, Logical>> {
        match self.get_node(key)? {
            NodeData::Workspace(workspace) => Some(workspace.geometry()),
            NodeData::Container(container) => Some(container.geometry()),
        }
    }

    /// Replace the pending node box without arranging its parent.
    ///
    /// Sway mutates these boxes directly during operations such as `container_swap` and an
    /// unfloat. Every node owns the same representation, so callers do not need to know whether
    /// a stable node key currently names a view or a split container.
    pub(in crate::layout) fn set_node_geometry(
        &mut self,
        key: NodeKey,
        geometry: Rectangle<f64, Logical>,
    ) -> bool {
        match self.get_node_mut(key) {
            Some(NodeData::Workspace(workspace)) => workspace.set_geometry(geometry),
            Some(NodeData::Container(container)) => container.set_geometry(geometry),
            None => return false,
        }
        true
    }

    /// Translate every pending box in a subtree without arranging it.
    ///
    /// Floating movement in sway is a recursive mutation: the root container, every nested
    /// split and every view/content box move by the same delta. Updating only the floating
    /// root target and relying on a later workspace arrange loses the movement whenever that
    /// branch is hidden behind another fullscreen subtree, because the arrange deliberately
    /// does not visit it.
    ///
    /// sway/tree/container.c:1128-1144 (`container_floating_translate`)
    pub(super) fn translate_subtree_geometry(&mut self, key: NodeKey, delta: Point<f64, Logical>) {
        let children = self
            .get_container(key)
            .map(|container| container.children().to_vec())
            .unwrap_or_default();

        if let Some(mut geometry) = self.node_geometry(key) {
            geometry.loc += delta;
            self.set_node_geometry(key, geometry);
        }

        if self.get_node(key).is_some_and(|node| node.is_view()) {
            if let Some(info) = self.leaf_layouts.get_mut(&key) {
                // The node geometry is sway's pending container box; `rect` is the corresponding
                // content/render box. `container_floating_translate` changes both.
                info.rect.loc += delta;
            }
        }

        for child in children {
            self.translate_subtree_geometry(child, delta);
        }
    }

    /// How far a node reaches along an axis — sway's `con->pending.width`/`.height`.
    pub(in crate::layout) fn node_span(&self, key: NodeKey, layout: LayoutMode) -> Option<f64> {
        let rect = self.node_geometry(key)?;
        match layout {
            LayoutMode::SplitH => Some(rect.size.w),
            LayoutMode::SplitV => Some(rect.size.h),
            LayoutMode::Tabbed | LayoutMode::Stacked => None,
        }
    }

    /// Get tile by key (O(1) access).
    pub(in crate::layout) fn get_tile(&self, key: NodeKey) -> Option<&Tile<W>> {
        match self.nodes.get(key)? {
            NodeData::Container(container) => container.tile(),
            _ => None,
        }
    }

    /// Get mutable tile by key (O(1) access).
    pub(in crate::layout) fn get_tile_mut(&mut self, key: NodeKey) -> Option<&mut Tile<W>> {
        match self.nodes.get_mut(key)? {
            NodeData::Container(container) => container.tile_mut(),
            _ => None,
        }
    }

    /// Permanently destroy a transferred subtree at the final unmap boundary.
    ///
    /// Reparenting never calls this: a move adopts the same transferred key into its destination.
    pub(in crate::layout) fn destroy_transferred_subtree(&mut self, key: NodeKey) -> bool {
        if !self.transfer_roots.remove(&key) {
            return false;
        }
        self.remove_node_recursive(key).is_some()
    }

    pub(in crate::layout) fn transferred_tile(&self, key: NodeKey) -> Option<&Tile<W>> {
        self.transfer_roots
            .contains(&key)
            .then(|| self.get_tile(key))
            .flatten()
    }

    pub(in crate::layout) fn transferred_tile_mut(&mut self, key: NodeKey) -> Option<&mut Tile<W>> {
        if !self.transfer_roots.contains(&key) {
            return None;
        }
        self.get_tile_mut(key)
    }

    pub(in crate::layout) fn transfer_root_keys(&self) -> impl Iterator<Item = NodeKey> + '_ {
        self.transfer_roots.iter().copied()
    }

    /// Register an already-materialized, parentless subtree as temporarily transferred.
    pub(super) fn register_transfer_root(&mut self, key: NodeKey) -> bool {
        if !self.nodes.contains_key(key) || self.parent_of(key).is_some() {
            return false;
        }
        self.transfer_roots.insert(key)
    }

    /// Hand a transferred subtree to a workspace and adopt that workspace's tile configuration.
    pub(super) fn consume_transfer_root(&mut self, destination: NodeKey, key: NodeKey) -> bool {
        if !self.transfer_roots.remove(&key) {
            return false;
        }
        self.adopt_tile_config(destination, key);
        true
    }

    /// Give every tile under `key` the box and options of the workspace it now belongs to.
    pub(super) fn adopt_tile_config(&mut self, destination: NodeKey, key: NodeKey) {
        let view_size = self.view_size(destination);
        let scale = self.scale(destination);
        let options = self.options(destination).clone();
        for leaf in self.leaf_keys_under(key) {
            if let Some(tile) = self.get_tile_mut(leaf) {
                tile.update_config(view_size, scale, options.clone());
            }
        }
    }

    /// Insert a node that has not belonged to a tree before.
    pub(super) fn insert_node(&mut self, workspace: NodeKey, node: NodeData<W>) -> NodeKey {
        assert!(
            !matches!(node, NodeData::Workspace(_)),
            "a workspace node is created with the tree"
        );
        let key = NodeKey::next();
        self.insert_node_with_key(workspace, key, node);
        key
    }

    /// Materialize a view that has not belonged to the tree before.
    pub(super) fn insert_view(&mut self, workspace: NodeKey, view: NewView<W>) -> NodeKey {
        let (key, container) = ContainerData::from_new_view(view);
        self.insert_node_with_key(workspace, key, NodeData::Container(container));
        key
    }

    /// Materialize one incoming view under `destination`'s configuration.
    ///
    /// Only a genuinely new map creates a node. An internal transfer adopts the transferred
    /// node in place, preserving every piece of node state and its position in the seat order.
    pub(super) fn materialize_incoming_view(
        &mut self,
        destination: NodeKey,
        view: super::IncomingView<W>,
    ) -> NodeKey {
        match view {
            super::IncomingView::New(view) => self.insert_view(destination, *view),
            super::IncomingView::Transferred(key)
                if self.consume_transfer_root(destination, key) =>
            {
                key
            }
            super::IncomingView::Transferred(key) => {
                panic!("an internal transfer must still own its node: {key:?}")
            }
        }
    }

    /// Put an existing node back into this workspace without changing its identity.
    ///
    /// A tile arrives holding the options of wherever it was built or last lived — `Monitor`
    /// builds one from `workspaces[0]` for whichever workspace is the target, and a subtree
    /// crossing to another workspace or another output brings its own along. Every workspace
    /// has its own handle on the options, and a named workspace's `layout_config` makes them
    /// differ in value too, so adoption belongs at the one place a node enters a tree
    /// rather than at each of the routes that lead here.
    pub(super) fn insert_node_with_key(
        &mut self,
        destination: NodeKey,
        key: NodeKey,
        mut node: NodeData<W>,
    ) {
        let window_id = node.as_tile().map(|tile| tile.window().id().clone());
        if let NodeData::Container(container) = &mut node {
            if let Some(tile) = container.tile_mut() {
                tile.update_config(
                    self.view_size(destination),
                    self.scale(destination),
                    self.options(destination).clone(),
                );
            }
        }
        self.nodes.insert(key, node, None);
        if let Some(window_id) = window_id {
            assert!(
                self.window_nodes.insert(window_id, key).is_none(),
                "a live window id can only name one tree node"
            );
        }
        self.seat.register(key);
    }

    /// Remove one node from the tree and retire every workspace authority naming it.
    ///
    /// Structural callers still own child handling and where focus should land next. What they
    /// do not own is whether the seat still lists the node: sway drops it from `focus_stack` in
    /// the destroy listener, so it happens once, at the removal, and cannot be forgotten by a
    /// caller who was thinking about something else. A stale entry would be worse than
    /// useless: a key that moved to another workspace still resolves, so the order would go on
    /// ranking it.
    ///
    /// sway/input/seat.c:261-324
    pub(super) fn remove_node_from_store(&mut self, key: NodeKey) -> Option<NodeData<W>> {
        let node = self.nodes.remove(key)?;
        if let Some(window_id) = node.as_tile().map(|tile| tile.window().id()) {
            assert_eq!(
                self.window_nodes.remove(window_id),
                Some(key),
                "the window index must retire the node leaving the store"
            );
        }
        self.transfer_roots.remove(&key);
        self.seat.unregister(key);
        // By value, not by asking the node which workspace it is on: a node on its way out of
        // the tree has usually been unparented already, and would answer with itself.
        let workspaces = self.workspaces.clone();
        for workspace in workspaces {
            // Removing a workspace also removes its authority with the workspace node. It has
            // already left `nodes` at this point, so there is nothing to clear or inspect.
            if workspace == key || self.get_workspace(workspace).is_none() {
                continue;
            }
            if self.workspace_data(workspace).fullscreen_key == Some(key) {
                self.workspace_data_mut(workspace).fullscreen_key = None;
            }
        }
        Some(node)
    }

    /// Remove a node from the tree (and recursively all its children).
    pub(super) fn remove_node_recursive(&mut self, key: NodeKey) -> Option<NodeData<W>> {
        // Read the children before the node leaves. A workspace lays out children exactly as a
        // split container does, so `get_container` covers both; matching on
        // `NodeData::Container` alone would leave a removed workspace's subtree in the store,
        // findable by `window_key` and owned by nothing.
        let children = self
            .get_container(key)
            .map(|container| container.children.clone())
            .unwrap_or_default();
        let node = self.remove_node_from_store(key)?;
        for child_key in children {
            self.remove_node_recursive(child_key);
        }
        Some(node)
    }

    /// Whether the workspace holds no windows.
    ///
    /// Asks for a leaf rather than for the root's children: the root is always there, and a
    /// container that has just been emptied can still be hanging off it when this is called.
    /// Every caller means "is there a window here".
    /// Whether one workspace's tiled side holds no window.
    ///
    /// Named for a workspace rather than for the tree: "empty" was the tree's answer while
    /// it held exactly one workspace, and every caller of it meant this.
    pub(in crate::layout) fn workspace_is_empty(&self, workspace: NodeKey) -> bool {
        self.first_leaf_key(workspace).is_none()
    }

    /// Whether `key` is `ancestor` or sits somewhere below it.
    pub(in crate::layout) fn is_descendant(&self, key: NodeKey, ancestor: NodeKey) -> bool {
        self.is_descendant_of(key, ancestor)
    }

    /// Whether `key` is `ancestor` or sits somewhere below it.
    pub(super) fn is_descendant_of(&self, key: NodeKey, ancestor: NodeKey) -> bool {
        let mut current = key;
        loop {
            if current == ancestor {
                return true;
            }
            match self.parent_of(current) {
                Some(parent) => current = parent,
                None => return false,
            }
        }
    }
}
