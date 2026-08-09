//! The workspace's floating side, in the same arena as its tiling side.
//!
//! sway's workspace holds two lists — `ws->tiling` and `ws->floating` — and the same
//! `sway_container` moves between them. `container_set_floating` detaches it from one and
//! attaches it to the other; nothing is destroyed, nothing is rebuilt, and every pointer
//! anyone was holding still points at it. That is why floating a window costs it nothing:
//! its place in the seat's focus order, its marks, its identity all survive because they were
//! never attached to which list it was in.
//!
//! Tiri kept the two sides in separate trees, each with its own slotmap. Crossing meant
//! taking a subtree apart and building it again from the other side's arena, with new keys —
//! so anything keyed by node identity was lost in transit, most visibly the focus order. This
//! module is the tiling tree learning to hold the floating side too, which is the first half
//! of making the crossing a move instead of a reconstruction.

use smithay::utils::{Logical, Rectangle};

use super::{ContainerTree, FloatingRoot, LayoutElement, NodeData, NodeKey};

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
    /// sway's `container_set_floating` in the direction that matters here: `container_detach`
    /// then `workspace_add_floating`. The node is the node throughout, so the seat's order,
    /// and anything else holding a key, is undisturbed — which is the whole point of doing it
    /// this way rather than by taking the subtree out and building it again.
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
        let old_parent = self.parent_of(key);
        self.detach_child(key);
        self.set_parent(key, None);
        self.floating_roots.push(FloatingRoot { key, area });
        if let Some(old_parent) = old_parent {
            self.reap_empty(old_parent);
        }
        true
    }

    /// The reverse: a floating root goes back into the tiled tree under `parent` at `index`.
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
            container.insert_child_unset(index, key);
        }
        self.set_parent(key, Some(parent));
        true
    }

    /// Drop a floating root that is going away.
    pub(in crate::layout) fn forget_floating_root(&mut self, key: NodeKey) {
        self.floating_roots.retain(|root| root.key != key);
    }
}
