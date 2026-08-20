//! Workspace-local node storage primitives: raw node access and parent links.

use smithay::utils::{Logical, Rectangle};

use super::ContainerArena;
use super::ContainerData;
use super::Layout;
use super::LayoutElement;
use super::LayoutParentData;
use super::NodeData;
use super::NodeKey;
use super::WorkspaceData;
use crate::layout::tile::Tile;

impl<W: LayoutElement> ContainerArena<W> {
    /// Whether this arena still holds a node.
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

    pub(super) fn get_workspace(&self) -> Option<&WorkspaceData> {
        match self.nodes.get(self.root)? {
            NodeData::Workspace(workspace) => Some(workspace),
            _ => None,
        }
    }

    pub(super) fn set_parent(&mut self, child: NodeKey, parent: Option<NodeKey>) {
        if let Some(entry) = self.parents.get_mut(child) {
            *entry = parent;
        } else {
            self.parents.insert(child, parent);
        }
    }

    pub(in crate::layout) fn parent_of(&self, key: NodeKey) -> Option<NodeKey> {
        self.parents.get(key).and_then(|parent| *parent)
    }

    pub(super) fn child_index(&self, parent_key: NodeKey, child_key: NodeKey) -> Option<usize> {
        self.get_container(parent_key)?
            .children
            .iter()
            .position(|&key| key == child_key)
    }

    /// Replace a child, handing its parent share to the replacement.
    ///
    /// `container_replace` inserts the replacement beside the old node, detaches the old one,
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
        if wrapper.child_count() != 0 {
            return None;
        }
        let raw_focus_returns_to_child = self.seat.node() == Some(child_key);
        self.child_index(parent_key, child_key)?;
        wrapper.set_geometry(self.node_geometry(child_key)?);

        let wrapper_key = self.insert_node(NodeData::Container(wrapper));
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
            // A view directly under tabs keeps the parent's whole pending box. The rendered
            // rectangle beside it has already had the title bar applied, so it cannot answer
            // resize or survive a reparent faithfully.
            //
            // sway/tree/arrange.c:185-211
            node if node.is_view() => Some(
                self.leaf_layouts
                    .iter()
                    .find(|info| info.key == key)
                    .map(|info| info.node_rect)
                    .unwrap_or_default(),
            ),
            NodeData::Container(container) => Some(container.geometry()),
        }
    }

    /// How far a node reaches along an axis — sway's `con->pending.width`/`.height`.
    pub(in crate::layout) fn node_span(&self, key: NodeKey, layout: Layout) -> Option<f64> {
        let rect = self.node_geometry(key)?;
        match layout {
            Layout::SplitH => Some(rect.size.w),
            Layout::SplitV => Some(rect.size.h),
            Layout::Tabbed | Layout::Stacked => None,
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

    /// Insert a node that has not belonged to a tree before.
    pub(super) fn insert_node(&mut self, node: NodeData<W>) -> NodeKey {
        let key = match &node {
            NodeData::Workspace(_) => panic!("a workspace node is created with the tree"),
            NodeData::Container(container) => container
                .tile()
                .map_or_else(NodeKey::next, |tile| tile.node_key()),
        };
        self.insert_node_with_key(key, node);
        key
    }

    /// Put an existing node back into this workspace without changing its identity.
    pub(super) fn insert_node_with_key(&mut self, key: NodeKey, node: NodeData<W>) {
        self.nodes.insert(key, node);
        self.parents.insert(key, None);
        self.seat.register(key);
    }

    /// Remove one node from the arena and retire every workspace authority naming it.
    ///
    /// Structural callers still own child handling and where focus should land next. What they
    /// do not own is whether the seat still lists the node: sway drops it from `focus_stack` in
    /// the destroy listener, so it happens once, at the removal, and cannot be forgotten by a
    /// caller who was thinking about something else. Twelve places remove a node here and only
    /// some of them used to say so, which left the order ranking keys that had moved to another
    /// workspace — where they still resolve, because the key is the same one.
    ///
    /// sway/input/seat.c:261-324
    pub(super) fn remove_node_from_store(&mut self, key: NodeKey) -> Option<NodeData<W>> {
        let node = self.nodes.remove(key)?;
        self.parents.remove(key);
        self.seat.unregister(key);
        if self.fullscreen_key == Some(key) {
            self.fullscreen_key = None;
        }
        Some(node)
    }

    /// Remove a node from this workspace store (and recursively all its children).
    pub(super) fn remove_node_recursive(&mut self, key: NodeKey) -> Option<NodeData<W>> {
        let node = self.remove_node_from_store(key)?;

        // If it's a container, recursively remove all children
        if let NodeData::Container(ref container) = node {
            for &child_key in &container.children {
                self.remove_node_recursive(child_key);
            }
        }

        Some(node)
    }

    /// Whether the workspace holds no windows.
    ///
    /// Asks for a leaf rather than for the root's children: the root is always there, and a
    /// container that has just been emptied can still be hanging off it when this is called.
    /// Every caller means "is there a window here".
    pub(in crate::layout) fn is_empty(&self) -> bool {
        self.first_leaf_key().is_none()
    }

    #[cfg(test)]
    pub(in crate::layout) fn root_is_workspace_node(&self) -> bool {
        matches!(self.get_node(self.root), Some(NodeData::Workspace(_)))
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
