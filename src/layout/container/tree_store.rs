//! Workspace-local node storage primitives: raw node access and parent links.

use smithay::utils::{Logical, Rectangle};

use super::ContainerData;
use super::ContainerTree;
use super::Layout;
use super::LayoutElement;
use super::NodeData;
use super::NodeKey;
use crate::layout::tile::Tile;

impl<W: LayoutElement> ContainerTree<W> {
    /// The implicit workspace-root container is an implementation detail and
    /// should be ignored in inactive-tiling reference resolution.
    pub(super) fn is_synthetic_root_container_key(&self, key: NodeKey) -> bool {
        self.root == key
    }

    /// Get node data by key
    pub(super) fn get_node(&self, key: NodeKey) -> Option<&NodeData<W>> {
        self.nodes.get(key)
    }

    /// Get mutable node data by key
    pub(super) fn get_node_mut(&mut self, key: NodeKey) -> Option<&mut NodeData<W>> {
        self.nodes.get_mut(key)
    }

    /// Get container data by key
    pub(super) fn get_container(&self, key: NodeKey) -> Option<&ContainerData> {
        match self.nodes.get(key)? {
            NodeData::Container(container) => Some(container),
            _ => None,
        }
    }

    /// Get mutable container data by key
    pub(super) fn get_container_mut(&mut self, key: NodeKey) -> Option<&mut ContainerData> {
        match self.nodes.get_mut(key)? {
            NodeData::Container(container) => Some(container),
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

    pub(super) fn parent_of(&self, key: NodeKey) -> Option<NodeKey> {
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
        let Some(fractions) = self.node_fractions(old_key) else {
            return false;
        };
        let Some(parent) = self.get_container_mut(parent_key) else {
            return false;
        };
        let Some(idx) = parent.children.iter().position(|key| *key == old_key) else {
            return false;
        };
        parent.children[idx] = new_key;
        self.set_node_fractions(new_key, fractions);
        self.set_parent(new_key, Some(parent_key));
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
        mut wrapper: ContainerData,
    ) -> Option<NodeKey> {
        if wrapper.child_count() != 0 {
            return None;
        }
        let raw_focus_returns_to_child = self.seat.node() == Some(child_key);
        self.child_index(parent_key, child_key)?;
        wrapper.set_geometry(self.node_geometry(child_key)?);

        let wrapper_key = self.insert_node(NodeData::Container(wrapper));
        if !self.replace_child_node(parent_key, child_key, wrapper_key) {
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
            NodeData::Container(container) => Some(container.geometry()),
            // A view directly under tabs keeps the parent's whole pending box. The rendered
            // rectangle beside it has already had the title bar applied, so it cannot answer
            // resize or survive a reparent faithfully.
            //
            // sway/tree/arrange.c:185-211
            NodeData::Leaf(_) => Some(
                self.leaf_layouts
                    .iter()
                    .find(|info| info.key == key)
                    .map(|info| info.node_rect)
                    .unwrap_or_default(),
            ),
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
            NodeData::Leaf(tile) => Some(tile),
            _ => None,
        }
    }

    /// Get mutable tile by key (O(1) access).
    pub(in crate::layout) fn get_tile_mut(&mut self, key: NodeKey) -> Option<&mut Tile<W>> {
        match self.nodes.get_mut(key)? {
            NodeData::Leaf(tile) => Some(tile),
            _ => None,
        }
    }

    /// Insert a node that has not belonged to a tree before.
    pub(super) fn insert_node(&mut self, node: NodeData<W>) -> NodeKey {
        let key = match &node {
            NodeData::Container(_) => NodeKey::next(),
            NodeData::Leaf(tile) => tile.node_key(),
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

    /// Remove a node from this workspace store (and recursively all its children).
    pub(super) fn remove_node_recursive(&mut self, key: NodeKey) -> Option<NodeData<W>> {
        let node = self.nodes.remove(key)?;
        self.parents.remove(key);

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

    pub(in crate::layout) fn root_is_synthetic_workspace_container(&self) -> bool {
        true
    }

    /// Parent of a node, or None for the root.
    pub(in crate::layout) fn parent_of_node(&self, key: NodeKey) -> Option<NodeKey> {
        self.parent_of(key)
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
