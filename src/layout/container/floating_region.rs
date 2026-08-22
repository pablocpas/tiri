//! The workspace's floating side, in the same arena as its tiling side.
//!
//! sway's workspace holds two lists — `ws->tiling` and `ws->floating` — and the same
//! `sway_container` moves between them. `container_set_floating` detaches it from one and
//! attaches it to the other; nothing is destroyed, nothing is rebuilt, and every pointer
//! anyone was holding still points at it. That is why floating a window costs it nothing:
//! its place in the seat's focus order, its marks, its identity all survive because they were
//! never attached to which list it was in.
//!
//! Tiri used to keep the two sides in separate trees, each with its own slotmap. Crossing
//! meant taking a subtree apart and building it again from the other side's arena, with new
//! keys — so anything keyed by node identity was lost in transit, most visibly the focus
//! order. This module made the two branches one workspace store; stable node identities make
//! the same guarantee continue through workspace and output moves.
//!
//! sway/commands/move.c:198-239

use smithay::utils::{Logical, Point, Rectangle, Size};

use super::{
    ContainerArena, ContainerData, FloatingGeometry, FloatingRoot, FloatingRootKind,
    InactiveTilingReference, Layout, LayoutElement, NodeData, NodeKey,
};
use crate::layout::tile::Tile;
use crate::layout::SizeFrac;

pub(in crate::layout) fn scale_floating_position(
    area: Rectangle<f64, Logical>,
    pos: Point<f64, SizeFrac>,
) -> Point<f64, Logical> {
    let mut logical_pos = Point::from((pos.x, pos.y));
    logical_pos.x *= area.size.w;
    logical_pos.y *= area.size.h;
    logical_pos + area.loc
}

pub(in crate::layout) fn floating_position_from_logical(
    area: Rectangle<f64, Logical>,
    logical_pos: Point<f64, Logical>,
) -> Point<f64, SizeFrac> {
    let pos = logical_pos - area.loc;
    let mut pos = Point::from((pos.x, pos.y));
    pos.x /= f64::max(area.size.w, 1.0);
    pos.y /= f64::max(area.size.h, 1.0);
    pos
}

impl FloatingGeometry {
    pub(super) fn new(
        working_area: Rectangle<f64, Logical>,
        area: Rectangle<f64, Logical>,
    ) -> Self {
        let mut geometry = Self {
            pos: floating_position_from_logical(working_area, area.loc),
            working_area,
            target: area,
            resize_base_size: area.size,
        };
        geometry.target.loc = geometry.logical_pos(area.size);
        geometry
    }

    fn logical_pos(&self, size: Size<f64, Logical>) -> Point<f64, Logical> {
        let mut logical_pos = scale_floating_position(self.working_area, self.pos);

        // Make sure the window doesn't go too much off-screen. Numbers taken from Mutter.
        let min_on_screen_hor = f64::clamp(size.w / 4., 10., 75.);
        let min_on_screen_ver = f64::clamp(size.h / 4., 10., 75.);
        let max_off_screen_hor = f64::max(0., size.w - min_on_screen_hor);
        let max_off_screen_ver = f64::max(0., size.h - min_on_screen_ver);

        logical_pos -= self.working_area.loc;
        logical_pos.x = f64::max(logical_pos.x, -max_off_screen_hor);
        logical_pos.y = f64::max(logical_pos.y, -max_off_screen_ver);
        logical_pos.x = f64::min(
            logical_pos.x,
            self.working_area.size.w - size.w + max_off_screen_hor,
        );
        logical_pos.y = f64::min(
            logical_pos.y,
            self.working_area.size.h - size.h + max_off_screen_ver,
        );
        logical_pos + self.working_area.loc
    }

    fn effective_area(&self) -> Rectangle<f64, Logical> {
        Rectangle::new(
            self.logical_pos(self.resize_base_size),
            self.resize_base_size,
        )
    }

    fn retarget_from_base(&mut self) -> Rectangle<f64, Logical> {
        let area = self.effective_area();
        self.target = area;
        area
    }
}

impl<W: LayoutElement> ContainerArena<W> {
    /// Floating roots in render order, topmost first.
    pub(in crate::layout) fn floating_roots(&self) -> impl Iterator<Item = NodeKey> + '_ {
        self.floating_roots.iter().map(|root| root.key)
    }

    pub(in crate::layout) fn floating_root_count(&self) -> usize {
        self.floating_roots.len()
    }

    pub(in crate::layout) fn floating_root_at(&self, index: usize) -> Option<NodeKey> {
        self.floating_roots.get(index).map(|root| root.key)
    }

    pub(in crate::layout) fn floating_root_id_at(&self, index: usize) -> Option<u64> {
        self.floating_roots.get(index).map(|root| root.id)
    }

    pub(in crate::layout) fn floating_root_index(&self, key: NodeKey) -> Option<usize> {
        self.floating_roots.iter().position(|root| root.key == key)
    }

    pub(in crate::layout) fn floating_root_index_by_id(&self, id: u64) -> Option<usize> {
        self.floating_roots.iter().position(|root| root.id == id)
    }

    pub(in crate::layout) fn floating_root_kind_at(
        &self,
        index: usize,
    ) -> Option<FloatingRootKind> {
        let root = self.floating_roots.get(index)?;
        if root.kind == FloatingRootKind::ImplicitWindowGroup
            && self
                .branch_container(root.key)
                .is_some_and(|container| container.is_user_container())
        {
            Some(FloatingRootKind::FloatedContainer)
        } else {
            Some(root.kind)
        }
    }

    pub(in crate::layout) fn floating_root_is_workspace_wrapper(&self, index: usize) -> bool {
        self.floating_roots
            .get(index)
            .is_some_and(|root| root.kind.is_workspace_wrapper())
    }

    pub(in crate::layout) fn move_floating_root(&mut self, from: usize, to: usize) -> bool {
        if from >= self.floating_roots.len() || to >= self.floating_roots.len() {
            return false;
        }
        let root = self.floating_roots.remove(from);
        self.floating_roots.insert(to, root);
        true
    }

    /// Every floating group with its box, for the arrange pass.
    pub(super) fn floating_roots_snapshot(&self) -> Vec<(NodeKey, Rectangle<f64, Logical>)> {
        self.floating_roots()
            .filter_map(|key| Some((key, self.floating_area(key)?)))
            .collect()
    }

    /// The box a floating group is laid out in.
    pub(in crate::layout) fn floating_area(&self, key: NodeKey) -> Option<Rectangle<f64, Logical>> {
        self.floating_roots
            .iter()
            .find(|root| root.key == key)
            .map(|root| root.geometry.target)
    }

    /// Effective box used to compose the next move or resize.
    pub(in crate::layout) fn floating_container_area(
        &self,
        key: NodeKey,
    ) -> Option<Rectangle<f64, Logical>> {
        Some(
            self.floating_roots
                .iter()
                .find(|root| root.key == key)?
                .geometry
                .effective_area(),
        )
    }

    pub(in crate::layout) fn floating_position(
        &self,
        key: NodeKey,
    ) -> Option<Point<f64, SizeFrac>> {
        Some(
            self.floating_roots
                .iter()
                .find(|root| root.key == key)?
                .geometry
                .pos,
        )
    }

    /// Move a floating root and retarget its layout from the same authoritative state.
    pub(in crate::layout) fn set_floating_logical_pos(
        &mut self,
        key: NodeKey,
        logical_pos: Point<f64, Logical>,
    ) -> Point<f64, Logical> {
        let (old_pos, new_pos) = {
            let geometry = self
                .floating_roots
                .iter_mut()
                .find(|root| root.key == key)
                .map(|root| &mut root.geometry)
                .expect("floating geometry can only be written for a floating root");
            let old_pos = geometry.effective_area().loc;
            geometry.pos = floating_position_from_logical(geometry.working_area, logical_pos);
            (old_pos, geometry.retarget_from_base().loc)
        };

        self.translate_subtree_geometry(key, new_pos - old_pos);
        new_pos
    }

    /// Resize a floating root and retarget its layout from the same authoritative state.
    pub(in crate::layout) fn set_floating_size(
        &mut self,
        key: NodeKey,
        size: Size<f64, Logical>,
    ) -> Rectangle<f64, Logical> {
        let geometry = self
            .floating_roots
            .iter_mut()
            .find(|root| root.key == key)
            .map(|root| &mut root.geometry)
            .expect("floating geometry can only be written for a floating root");
        geometry.resize_base_size = size;
        geometry.retarget_from_base()
    }

    /// Re-anchor a root after its output working area changes.
    pub(in crate::layout) fn update_floating_working_area(
        &mut self,
        key: NodeKey,
        working_area: Rectangle<f64, Logical>,
    ) -> Rectangle<f64, Logical> {
        let geometry = self
            .floating_roots
            .iter_mut()
            .find(|root| root.key == key)
            .map(|root| &mut root.geometry)
            .expect("a floating stack entry must name a floating root");
        geometry.working_area = working_area;
        geometry.retarget_from_base()
    }

    /// Record what a one-window client commit contributes to the next resize.
    ///
    /// This must not replace `geometry`: a client can commit an older or deliberately different
    /// size while a newer compositor target still needs to be requested.
    pub(in crate::layout) fn record_floating_resize_base(
        &mut self,
        key: NodeKey,
        size: Size<f64, Logical>,
    ) {
        self.floating_roots
            .iter_mut()
            .find(|root| root.key == key)
            .map(|root| &mut root.geometry)
            .expect("a resize base can only be recorded for a floating root")
            .resize_base_size = size;
    }

    /// Whether this node is one of the workspace's floating roots.
    ///
    /// The node that is *in* `ws->floating`, and nothing inside it. A view is one as readily as
    /// a split: sway floats whatever it was given.
    pub(in crate::layout) fn is_floating_root(&self, key: NodeKey) -> bool {
        self.floating_roots.iter().any(|root| root.key == key)
    }

    /// Whether a node belongs to one of the workspace's floating branches.
    ///
    /// This is sway's `container_is_floating_or_child`: walk to the top-level ancestor and ask
    /// whether that ancestor is one of the workspace's floating roots. Keep it distinct from
    /// `is_floating_root`, because commands such as directional move treat a floating root and
    /// one of its descendants differently.
    pub(in crate::layout) fn is_in_floating_branch(&self, key: NodeKey) -> bool {
        self.is_floating_root(self.branch_root(key))
    }

    /// The layout branch that owns a node: the workspace's tiled side or one floating group.
    ///
    /// Floating roots have the workspace as their semantic parent, but they are not children in
    /// its tiled list. Stop at the direct child carrying floating geometry instead of walking
    /// through that semantic edge.
    pub(in crate::layout) fn branch_root(&self, key: NodeKey) -> NodeKey {
        let mut current = key;
        while let Some(parent) = self.parent_of(current) {
            if parent == self.root {
                return if self.is_floating_root(current) {
                    current
                } else {
                    self.root
                };
            }
            current = parent;
        }
        current
    }

    fn register_floating_root(
        &mut self,
        key: NodeKey,
        area: Rectangle<f64, Logical>,
        stack_index: usize,
        kind: FloatingRootKind,
    ) {
        assert_ne!(key, self.root, "the workspace root cannot become floating");
        assert_eq!(
            self.parent_of(key),
            None,
            "a floating root must be detached before registration"
        );
        assert!(
            self.get_any_container(key).is_some(),
            "a floating root must exist in the arena"
        );
        assert!(
            !self.is_floating_root(key),
            "a floating root can only be registered once"
        );
        let root = FloatingRoot {
            id: self.next_floating_root_id,
            key,
            kind,
            geometry: FloatingGeometry::new(self.working_area, area),
        };
        self.next_floating_root_id += 1;
        self.floating_roots
            .insert(stack_index.min(self.floating_roots.len()), root);
        self.set_parent(key, Some(self.root));
    }

    /// Move a subtree from the tiled side to the floating side, keeping its identity.
    ///
    /// sway's `container_set_floating` does `container_detach` then
    /// `workspace_add_floating` (`sway/tree/container.c:1004-1038`). The node is the node
    /// throughout, so the seat's order, and anything else holding a key, is undisturbed —
    /// which is the whole point of doing it this way rather than by taking the subtree out and
    /// building it again.
    ///
    /// Returns false when there is nothing to float: the workspace root itself, a leaf (which
    /// must first go through `float_as_group`), or a key the tree does not hold. Requiring a
    /// container root gives every floating group one authoritative geometry field.
    pub(in crate::layout) fn float_subtree(
        &mut self,
        key: NodeKey,
        area: Rectangle<f64, Logical>,
        stack_index: usize,
        kind: FloatingRootKind,
    ) -> bool {
        if key == self.root
            || !matches!(self.get_node(key), Some(NodeData::Container(_)))
            || self.is_in_floating_branch(key)
        {
            return false;
        }
        self.discard_layout_superseded_by_transfer();
        let old_parent = self.parent_of(key);
        let restore_raw_focus = (self.seat.node() == Some(key))
            .then_some(old_parent)
            .flatten();
        self.detach_child(key);
        self.register_floating_root(key, area, stack_index, kind);
        if let Some(old_parent) = restore_raw_focus {
            // A focused container keeps focus when it changes workspace lists. Sway does not
            // focus its new ancestry: it raises the old parent and then the moved node with
            // `seat_set_raw_focus`, preserving the exact inactive-tiling reference that a
            // later unfloat reads from the global focus stack.
            //
            // sway/tree/container.c:1030-1035
            self.seat.raw_focus(old_parent);
            self.seat.raw_focus(key);
        }
        if let Some(old_parent) = old_parent {
            self.reap_empty(old_parent);
        }
        // A workspace fullscreen arrange may skip the new floating branch entirely. Move its
        // cached leaves to their new branch address now so that arranging the old tiled branch
        // does not discard them as stale tiled entries before the floating pending boxes can be
        // observed.
        self.readdress_leaf_layouts();
        true
    }

    /// The reverse half of `container_set_floating` moves the same node back under the inactive
    /// tiling reference (`sway/tree/container.c:1039-1057`).
    pub(in crate::layout) fn unfloat_subtree(
        &mut self,
        key: NodeKey,
        parent: NodeKey,
        index: usize,
    ) -> bool {
        if key == self.root || self.parent_of(key) != Some(self.root) || !self.is_floating_root(key)
        {
            return false;
        }
        if self.get_container(parent).is_none() {
            return false;
        }
        let root_idx = self
            .floating_root_index(key)
            .expect("a floating root must have exactly one stack entry");
        self.floating_roots.remove(root_idx);
        if let Some(container) = self.get_container_mut(parent) {
            let index = index.min(container.child_count());
            container.insert_child(index, key);
        }
        self.set_parent(key, Some(parent));
        // Enabling floating leaves the old fractions on the container. Disabling it clears
        // them after the container has rejoined `ws->tiling`, because its former share in the
        // floating list says nothing about the new siblings.
        //
        // sway/tree/container.c:1004-1074
        self.unset_node_fractions(key);
        true
    }

    /// Drop a floating root that is going away.
    pub(in crate::layout) fn forget_floating_root(&mut self, key: NodeKey) {
        if let Some(index) = self.floating_root_index(key) {
            self.floating_roots.remove(index);
        }
        if self.branch_is_empty(key) {
            self.remove_node_from_store(key);
            self.prune_focus_order();
            self.prune_selected_key();
        }
    }

    /// Float a node as a group of its own, and answer with the node the group is addressed by.
    ///
    /// sway floats the container it is given, and a lone view is already a container there.
    /// Tiri's floating side addresses a group as a container — `focus parent` stops on it, a
    /// split aims at it — so a lone window has one built around it here. The window's own key
    /// is untouched either way, which is the whole point of the crossing being a move.
    pub(in crate::layout) fn float_as_group(
        &mut self,
        key: NodeKey,
        area: Rectangle<f64, Logical>,
        stack_index: usize,
        kind: FloatingRootKind,
    ) -> Option<NodeKey> {
        if key == self.root {
            return None;
        }
        // A view floats as itself. sway's `ws->floating` holds whatever was floated, a view
        // included, so there is no wrapper to build and no extra level for a layout command
        // to hit.
        self.float_subtree(key, area, stack_index, kind)
            .then_some(key)
    }

    /// Put a new container in a floating root's place, with the old root inside it.
    ///
    /// `container_split` builds this wrapper the same way for a floating root as for anything
    /// else: `container_replace` inserts it beside the child in whatever list holds the child —
    /// `ws->floating` here — and then detaches the child into it
    /// (sway/tree/container.c:1547-1554, :1605-1615). So what moves is the slot, and the
    /// geometry this tree keeps on the root is that slot; the old root becomes an ordinary
    /// child of the wrapper and answers for nothing above it.
    ///
    /// Answers with the new root. The authoritative stack entry is replaced in the same
    /// operation, so no caller has a second list to synchronize.
    pub(in crate::layout) fn wrap_floating_root_in_new_container(
        &mut self,
        key: NodeKey,
        mut wrapper: ContainerData<W>,
    ) -> Option<NodeKey> {
        if wrapper.child_count() != 0 {
            return None;
        }
        let root_idx = self.floating_root_index(key)?;
        wrapper.set_geometry(self.node_geometry(key)?);

        let raw_focus_returns_to_child = self.seat.node() == Some(key);
        let wrapper_key = self.insert_node(NodeData::Container(wrapper));
        self.floating_roots[root_idx].key = wrapper_key;
        self.set_parent(wrapper_key, Some(self.root));
        self.get_container_mut(wrapper_key)
            .expect("the wrapper was just inserted")
            .insert_child(0, key);
        self.set_parent(key, Some(wrapper_key));
        self.transfer_fullscreen_to_replacement(key, wrapper_key);

        // Same tail as `wrap_child_in_new_container`: a container the seat has never heard of
        // answers for nothing (sway/tree/container.c:1616-1623).
        if self.selected_key() == Some(key) {
            self.seat.keep_selected(key);
        }
        if raw_focus_returns_to_child {
            self.seat.raw_focus(wrapper_key);
            self.seat.raw_focus(key);
        }
        Some(wrapper_key)
    }

    /// Float a window that was never in the tree.
    ///
    /// Returns the floating root and the view, which are now the same node.
    pub(in crate::layout) fn float_new_group(
        &mut self,
        tile: Tile<W>,
        area: Rectangle<f64, Logical>,
        stack_index: usize,
    ) -> (NodeKey, NodeKey) {
        // A configure outstanding for this tile's old tiled box cannot govern the branch it
        // is about to become the whole of, the same reason `float_subtree` drops one. Without
        // this the arrange that would give the new group its leaves' boxes is deferred behind
        // a transaction that describes a layout the tile has already left, and the group sits
        // on the workspace, focused, with nothing on screen.
        self.discard_layout_superseded_by_transfer();
        let leaf = self.insert_node(NodeData::Container(ContainerData::new_view(tile)));
        self.register_floating_root(
            leaf,
            area,
            stack_index,
            FloatingRootKind::ImplicitWindowGroup,
        );
        (leaf, leaf)
    }

    /// The child a group container holds when the container is only tiri's way of addressing
    /// one window, rather than an arrangement the user asked for.
    ///
    /// Which of the two it is was decided when the root was registered, and that record is
    /// the answer. Re-deriving it from the shape — one child, a split layout, no user flag —
    /// gets a container the user did ask for wrong whenever it happens to hold a single
    /// window, which is every container `layout` builds around a lone view. Unwrapping one
    /// of those on the way back loses a level sway keeps
    /// (`tiri-parity/fixtures/unfloat-a-selected-container.parity`).
    fn implicit_group_child(&self, group: NodeKey) -> Option<NodeKey> {
        let index = self.floating_root_index(group)?;
        if self.floating_root_kind_at(index)? != FloatingRootKind::ImplicitWindowGroup {
            return None;
        }
        let container = self.get_real_container(group)?;
        if container.child_count() != 1
            || !matches!(container.layout(), Layout::SplitH | Layout::SplitV)
        {
            return None;
        }
        container.child_key(0)
    }

    /// Return a floating group to the tiled side, under `parent` at `index`.
    ///
    /// A group that is only the container built around one window goes back as the window:
    /// materialising that container in the tiling would add a level sway does not have, and
    /// the tiling side used to have to recognise and remove it on arrival.
    ///
    /// Answers with the node that landed in the tiling.
    pub(in crate::layout) fn unfloat_group(
        &mut self,
        group: NodeKey,
        parent: NodeKey,
        index: usize,
    ) -> Option<NodeKey> {
        let Some(child) = self.implicit_group_child(group) else {
            return self.unfloat_subtree(group, parent, index).then_some(group);
        };

        self.detach_child(child);
        self.forget_floating_root(group);
        self.remove_node_from_store(group);

        let container = self.get_container_mut(parent)?;
        let index = index.min(container.child_count());
        container.insert_child(index, child);
        self.set_parent(child, Some(parent));
        self.unset_node_fractions(child);
        self.prune_focus_order();
        Some(child)
    }

    /// Move one node out of a floating group and back under the workspace.
    ///
    /// This is the view-targeted form of `container_set_floating(false)`. A leaf in a group
    /// is detached without rebuilding it; when that empties the implicit group wrapper, only
    /// the wrapper goes away. The leaf's key is the same key on both sides.
    ///
    /// Returns whether the floating group became empty and was removed.
    pub(in crate::layout) fn unfloat_node(
        &mut self,
        key: NodeKey,
        reference: Option<&InactiveTilingReference>,
    ) -> Option<bool> {
        if !self.is_in_floating_branch(key) {
            return None;
        }
        let group = self.branch_root(key);
        if key == group {
            let changed = match reference {
                Some(reference) => self.unfloat_with_tiling_reference(group, reference),
                None => self.unfloat_into_workspace(group),
            };
            return changed.then_some(true);
        }

        let old_parent = self.parent_of(key)?;
        self.detach_child(key);
        self.set_parent(key, None);
        self.reap_empty(old_parent);

        let group_empty = self.branch_is_empty(group);
        if group_empty {
            self.forget_floating_root(group);
            self.remove_node_from_store(group);
        }

        let workspace = self.root;
        let (parent, index) = reference
            .and_then(|reference| self.tiling_insertion_point(reference))
            .unwrap_or_else(|| (workspace, self.branch_children_len(workspace)));
        self.insert_key_into_branch(parent, index, key, false);
        self.unset_node_fractions(key);
        self.refresh_focus_visibility();
        Some(group_empty)
    }

    /// Float the whole tiled side as one group.
    ///
    /// sway's `floating toggle` with the workspace selected: `workspace_wrap_children` builds
    /// a container holding everything and `container_set_floating` moves that container to the
    /// other list. The workspace itself is never destroyed, which is why the wrapper carries
    /// its layout out and pours it back in on the way home.
    pub(in crate::layout) fn float_whole_workspace(
        &mut self,
        area: Rectangle<f64, Logical>,
        stack_index: usize,
    ) -> Option<NodeKey> {
        self.first_leaf_key()?;
        let layout = self.root_container_layout();
        let prev_split_layout = self
            .get_container(self.root)
            .and_then(|workspace| workspace.prev_split_layout());
        // `cmd_floating` first lets `workspace_wrap_children` copy the current workspace
        // layout onto the new container, then resets the now-empty workspace to `L_HORIZ`.
        // The two values are deliberately different when tabs/stacks or a vertical split
        // were selected: the wrapper carries that layout into floating while the workspace
        // goes back to its canonical horizontal orientation.
        let wrapper = self.wrap_workspace_children(layout, Layout::SplitH)?;
        if let Some(container) = self.get_container_mut(wrapper) {
            container.prev_split_layout = prev_split_layout;
        }
        // `cmd_floating` explicitly focuses the wrapper it just made before handing it to
        // `container_set_floating`. This is the one transfer whose target did not exist when
        // command focus was resolved, so it cannot rely on the common focus-preserving move.
        self.select_container(wrapper);
        self.float_subtree(
            wrapper,
            area,
            stack_index,
            FloatingRootKind::WorkspaceWrapper,
        )
        .then_some(wrapper)
    }

    /// Put a floating group back beside or inside the exact tiling node selected by the seat.
    pub(in crate::layout) fn unfloat_with_tiling_reference(
        &mut self,
        group: NodeKey,
        reference: &InactiveTilingReference,
    ) -> bool {
        match self.tiling_insertion_point(reference) {
            Some((parent, index)) => {
                if let Some(size) = self.node_geometry(reference.key()).map(|rect| rect.size) {
                    self.seed_unfloated_size(group, size);
                }
                self.place_unfloated(group, parent, index)
            }
            None => self.unfloat_into_workspace(group),
        }
    }

    /// Put a floating group back at the end of the workspace's own children.
    pub(in crate::layout) fn unfloat_into_workspace(&mut self, group: NodeKey) -> bool {
        let root = self.root;
        if let Some(size) = self.node_geometry(root).map(|rect| rect.size) {
            self.seed_unfloated_size(group, size);
        }
        let end = self.branch_children_len(root);
        self.place_unfloated(group, root, end)
    }

    /// Put back the wrapper that was built when the workspace itself was floated.
    ///
    /// Sway creates this container in `workspace_wrap_children` and disabling floating moves
    /// that same container back. It remains addressable even with one child.
    pub(in crate::layout) fn unfloat_as_workspace(&mut self, group: NodeKey) -> bool {
        let root = self.root;
        if let Some(size) = self.node_geometry(root).map(|rect| rect.size) {
            self.seed_unfloated_size(group, size);
        }
        if !self.unfloat_subtree(group, root, 0) {
            return false;
        }
        // The wrapper and every descendant kept their NodeKeys. `container_set_floating(false)`
        // does not touch the seat, so both leaf focus and a selected descendant survive as-is.
        self.refresh_focus_visibility();
        true
    }

    /// `container_set_floating(false)` copies only the destination's pending size onto the
    /// returning node. Its position is deliberately left untouched; workspace fullscreen may
    /// then arrange only a descendant and expose this half-updated parent exactly as Sway does.
    fn seed_unfloated_size(&mut self, group: NodeKey, size: Size<f64, Logical>) {
        let Some(mut geometry) = self.node_geometry(group) else {
            return;
        };
        geometry.size = size;
        self.set_node_geometry(group, geometry);
    }

    fn place_unfloated(&mut self, group: NodeKey, parent: NodeKey, index: usize) -> bool {
        let Some(_) = self.unfloat_group(group, parent, index) else {
            return false;
        };
        // This is one arena, so nothing needs reconstructing after the list change. Sway's
        // returning half of `container_set_floating` has no seat call at all.
        self.refresh_focus_visibility();
        true
    }
}
