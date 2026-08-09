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

use smithay::utils::{Logical, Rectangle};

use super::{
    ContainerData, ContainerTree, FloatingRoot, InsertParentInfo, Layout, LayoutElement, NodeData,
    NodeKey,
};
use crate::layout::tile::Tile;

impl<W: LayoutElement> ContainerTree<W> {
    /// The roots of the floating groups — sway's `ws->floating`.
    pub(in crate::layout) fn floating_roots(&self) -> impl Iterator<Item = NodeKey> + '_ {
        self.floating_roots.iter().map(|root| root.key)
    }

    /// Every floating group with its box, for the arrange pass.
    pub(super) fn floating_roots_snapshot(&self) -> Vec<FloatingRoot> {
        self.floating_roots.clone()
    }

    /// The box a floating group is laid out in.
    pub(in crate::layout) fn floating_area(&self, key: NodeKey) -> Option<Rectangle<f64, Logical>> {
        self.floating_roots
            .iter()
            .find(|root| root.key == key)
            .map(|root| root.area)
    }

    /// Move a floating group, or resize it.
    pub(in crate::layout) fn set_floating_area(
        &mut self,
        key: NodeKey,
        area: Rectangle<f64, Logical>,
    ) -> bool {
        match self.floating_roots.iter_mut().find(|root| root.key == key) {
            Some(root) => {
                root.area = area;
                true
            }
            None => false,
        }
    }

    /// Whether a node is floating, which in sway is a question about ancestry.
    ///
    /// `container_is_floating` walks up to the top and asks which of the workspace's two lists
    /// the topmost ancestor is in. Same here: a node is floating when the root of its branch
    /// is one of the floating ones.
    pub(in crate::layout) fn is_floating(&self, key: NodeKey) -> bool {
        let branch = self.branch_root(key);
        self.floating_roots.iter().any(|root| root.key == branch)
    }

    /// The topmost ancestor of a node — the workspace root, or a floating root.
    pub(in crate::layout) fn branch_root(&self, key: NodeKey) -> NodeKey {
        let mut current = key;
        while let Some(parent) = self.parent_of(current) {
            current = parent;
        }
        current
    }

    /// Move a subtree from the tiled side to the floating side, keeping its identity.
    ///
    /// sway's `container_set_floating` does `container_detach` then
    /// `workspace_add_floating` (`sway/tree/container.c:1004-1038`). The node is the node
    /// throughout, so the seat's order, and anything else holding a key, is undisturbed —
    /// which is the whole point of doing it this way rather than by taking the subtree out and
    /// building it again.
    ///
    /// Returns false when there is nothing to float: the workspace root itself, or a key the
    /// tree does not hold.
    pub(in crate::layout) fn float_subtree(
        &mut self,
        key: NodeKey,
        area: Rectangle<f64, Logical>,
    ) -> bool {
        if key == self.root || !self.nodes.contains_key(key) || self.is_floating(key) {
            return false;
        }
        self.discard_layout_superseded_by_transfer();
        let old_parent = self.parent_of(key);
        self.detach_child(key);
        self.set_parent(key, None);
        self.floating_roots.push(FloatingRoot { key, area });
        if let Some(old_parent) = old_parent {
            self.reap_empty(old_parent);
        }
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
        let Some(position) = self.floating_roots.iter().position(|root| root.key == key) else {
            return false;
        };
        if !matches!(self.get_node(parent), Some(NodeData::Container(_))) {
            return false;
        }
        self.floating_roots.remove(position);
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
        self.floating_roots.retain(|root| root.key != key);
        if self.branch_is_empty(key) {
            self.nodes.remove(key);
            self.parents.remove(key);
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
    ) -> Option<NodeKey> {
        let group = match self.get_node(key)? {
            NodeData::Container(_) => key,
            NodeData::Leaf(_) => {
                let parent = self.parent_of(key)?;
                self.wrap_child_in_new_container(parent, key, ContainerData::new(Layout::SplitH))?
            }
        };
        self.float_subtree(group, area).then_some(group)
    }

    /// Build a floating group around a window that was never in the tree.
    ///
    /// Returns the group's root and the leaf inside it.
    pub(in crate::layout) fn float_new_group(
        &mut self,
        tile: Tile<W>,
        area: Rectangle<f64, Logical>,
    ) -> (NodeKey, NodeKey) {
        let group = self.insert_node(NodeData::Container(ContainerData::new(Layout::SplitH)));
        self.set_parent(group, None);
        self.floating_roots.push(FloatingRoot { key: group, area });
        let leaf = self.insert_node(NodeData::Leaf(tile));
        if let Some(container) = self.get_container_mut(group) {
            container.insert_child(0, leaf);
        }
        self.set_parent(leaf, Some(group));
        (group, leaf)
    }

    /// The child a group container holds when the container is only tiri's way of addressing
    /// one window, rather than an arrangement the user asked for.
    fn implicit_group_child(&self, group: NodeKey) -> Option<NodeKey> {
        let container = self.get_container(group)?;
        if container.child_count() != 1
            || container.is_user_container()
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
        self.nodes.remove(group);
        self.parents.remove(group);

        if !matches!(self.get_node(parent), Some(NodeData::Container(_))) {
            return None;
        }
        if let Some(container) = self.get_container_mut(parent) {
            let index = index.min(container.child_count());
            container.insert_child(index, child);
        }
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
        info: Option<&InsertParentInfo>,
        focus: bool,
    ) -> Option<bool> {
        if !self.is_floating(key) {
            return None;
        }
        let group = self.branch_root(key);
        if key == group {
            let changed = match info {
                Some(info) => self.unfloat_with_parent_info(group, info, focus),
                None => self.unfloat_into_workspace(group, focus),
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
            self.nodes.remove(group);
            self.parents.remove(group);
        }

        let workspace = self.root;
        let (parent, index) = info
            .and_then(|info| {
                let parent =
                    self.ensure_container_at_path(workspace, &info.parent_path, info.layout)?;
                Some((parent, info.insert_idx))
            })
            .unwrap_or_else(|| (workspace, self.branch_children_len(workspace)));
        self.insert_key_into_branch(parent, index, key, focus);
        self.unset_node_fractions(key);
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
    ) -> Option<NodeKey> {
        self.first_leaf_key()?;
        let layout = self.root_container_layout();
        let prev_split_layout = self
            .get_container(self.root)
            .and_then(ContainerData::prev_split_layout);
        let wrapper = self.wrap_workspace_children(layout, layout)?;
        if let Some(container) = self.get_container_mut(wrapper) {
            container.prev_split_layout = prev_split_layout;
        }
        self.float_subtree(wrapper, area).then_some(wrapper)
    }

    /// Put a floating group back where `info` says it came from.
    pub(in crate::layout) fn unfloat_with_parent_info(
        &mut self,
        group: NodeKey,
        info: &InsertParentInfo,
        focus: bool,
    ) -> bool {
        let root = self.root;
        match self.ensure_container_at_path(root, &info.parent_path, info.layout) {
            Some(container_key) => {
                self.place_unfloated(group, container_key, info.insert_idx, focus)
            }
            None => self.unfloat_into_workspace(group, focus),
        }
    }

    /// Put a floating group back at the end of the workspace's own children.
    pub(in crate::layout) fn unfloat_into_workspace(
        &mut self,
        group: NodeKey,
        focus: bool,
    ) -> bool {
        let root = self.root;
        let end = self.branch_children_len(root);
        self.place_unfloated(group, root, end, focus)
    }

    /// Put back a group that *was* the workspace: its contents become the workspace's again.
    pub(in crate::layout) fn unfloat_as_workspace(&mut self, group: NodeKey, focus: bool) -> bool {
        let root = self.root;
        if !self.unfloat_subtree(group, root, 0) {
            return false;
        }
        let landed = if self.absorb_lone_child_into_branch_root(root) {
            self.first_leaf_key()
        } else {
            Some(group)
        };
        match landed {
            Some(key) => self.settle_focus_after_insert(key, focus),
            None => self.resync_focus(),
        }
        true
    }

    fn place_unfloated(
        &mut self,
        group: NodeKey,
        parent: NodeKey,
        index: usize,
        focus: bool,
    ) -> bool {
        let Some(landed) = self.unfloat_group(group, parent, index) else {
            return false;
        };
        self.settle_focus_after_insert(landed, focus);
        true
    }

    /// Take a branch root's only child's place: its layout, its children, its shares.
    ///
    /// The workspace outlives what is in it, so a subtree that *was* the workspace cannot be
    /// put back by making it the root — it has to be poured into the root that stayed. sway
    /// unfloating a wrapped workspace does the same thing from the other end, with
    /// `container_replace` on a workspace it never destroyed.
    pub(in crate::layout) fn absorb_lone_child_into_branch_root(
        &mut self,
        branch_root: NodeKey,
    ) -> bool {
        let Some(child) = self
            .get_container(branch_root)
            .filter(|root| root.child_count() == 1)
            .and_then(|root| root.child_key(0))
        else {
            return false;
        };
        let Some(container) = self.get_container(child) else {
            return false;
        };

        let layout = container.layout();
        let children = container.children().to_vec();
        let prev_split_layout = container.prev_split_layout();

        if let Some(root) = self.get_container_mut(branch_root) {
            root.set_layout(layout);
            root.children = children.clone();
            root.prev_split_layout = prev_split_layout;
        }
        for grandchild in children {
            self.set_parent(grandchild, Some(branch_root));
        }
        self.nodes.remove(child);
        self.parents.remove(child);
        self.prune_focus_order();
        true
    }
}
