//! Everything the seat knows about focus, and the only place it can be changed.
//!
//! The fields are private to this module on purpose: focusing something and updating the
//! order must not be separable. When they can be, the order ends up right for some commands
//! and quietly wrong for the rest, and the symptoms all look different — a window focused but
//! not shown, a tab that will not change, a `focus next` landing one window off.
//!
//! sway does not have this problem because it has no place to make the mistake:
//! `seat_set_focus` is a function, the stack is behind it, and there is nothing else to
//! assign. This is that shape.

use super::{LayoutElement, NodeKey, NodeStore};

/// The seat's focus, as sway keeps it.
#[derive(Debug, Default)]
pub(super) struct SeatFocus {
    /// Every node, most recently focused first — sway's `sway_seat::focus_stack`.
    ///
    /// One list, not one per container. `seat_get_active_tiling_child` reads which tab a
    /// switcher shows straight off it, taking the first entry whose *direct parent* is that
    /// switcher, so moving a node between containers changes the answer without anything
    /// having to be told.
    order: Vec<NodeKey>,
    /// The leaf holding keyboard focus.
    ///
    /// sway's seat focus is a node, container or view alike, and the keyboard follows it down
    /// through `seat_get_focus_inactive_view`. Tiri needs the view itself often enough to
    /// keep it here rather than re-derive it.
    leaf: Option<NodeKey>,
    /// The container `focus parent` selected, when one is selected.
    selected: Option<NodeKey>,
    /// Counts every time something is focused or selected.
    ///
    /// For state that holds "until focus next changes" without every focus change having to
    /// remember to clear it.
    generation: u64,
}

impl SeatFocus {
    pub(super) fn focused_leaf(&self) -> Option<NodeKey> {
        self.leaf
    }

    pub(super) fn selected(&self) -> Option<NodeKey> {
        self.selected
    }

    pub(super) fn order(&self) -> &[NodeKey] {
        &self.order
    }

    pub(super) fn generation(&self) -> u64 {
        self.generation
    }

    /// The node sway's seat is focused on: a selected container, or the keyboard-focus leaf.
    pub(super) fn node(&self) -> Option<NodeKey> {
        self.selected.or(self.leaf)
    }

    /// sway's new-node listener appends every node to the focus stack. A node that has never
    /// been focused still has a defined place behind every node the seat already knew.
    pub(super) fn register(&mut self, key: NodeKey) {
        if !self.order.contains(&key) {
            self.order.push(key);
        }
    }

    pub(super) fn unregister(&mut self, key: NodeKey) {
        self.order.retain(|entry| *entry != key);
        if self.leaf == Some(key) {
            self.leaf = None;
        }
        if self.selected == Some(key) {
            self.selected = None;
        }
    }

    /// sway's `seat_set_raw_focus`: raise one node and no ancestors, without changing which
    /// node owns keyboard focus (`sway/input/seat.c:1115-1126`).
    pub(super) fn raw_focus(&mut self, key: NodeKey) {
        self.order.retain(|entry| *entry != key);
        self.order.insert(0, key);
    }

    /// sway's `seat_set_focus`: the node becomes the most recent, and its ancestry with it.
    ///
    /// `chain` is the node first, followed by its parent up to the workspace. sway raw-focuses
    /// those parents from the inside out, then the workspace, and finally the node. Since raw
    /// focus inserts at the front, the resulting order is the node, the workspace, and the
    /// ancestors from outside in. A container sits ahead of its siblings exactly when
    /// something inside it was focused more recently than anything inside them.
    ///
    /// What is raised is what was *focused*. Focusing a container raises the container, not
    /// a window underneath it: the windows in there keep the order they had, and a later
    /// descent still knows which of them was in front.
    pub(super) fn focus(&mut self, chain: &[NodeKey], leaf: Option<NodeKey>) {
        self.raise(chain);
        self.leaf = leaf;
        self.selected = None;
        self.generation += 1;
    }

    /// The same, for a container `focus parent` left selected.
    pub(super) fn select(&mut self, chain: &[NodeKey], container: NodeKey, leaf: Option<NodeKey>) {
        self.raise(chain);
        self.leaf = leaf;
        self.selected = Some(container);
        self.generation += 1;
    }

    /// Keyboard focus moves; the order does not.
    ///
    /// One outcome needs this and no other: `cmd_move` leaves the moved view holding focus
    /// while every switcher goes on showing whatever it was showing, because the move changes
    /// the node's parent and touches the seat not at all.
    pub(super) fn follow_without_raising(&mut self, leaf: NodeKey) {
        self.leaf = Some(leaf);
    }

    pub(super) fn clear(&mut self) {
        self.leaf = None;
        self.selected = None;
    }

    /// The selected container is going away; point the selection at whatever replaces it.
    ///
    /// Deliberately does not raise. Nothing has been focused — a normalization is destroying
    /// the node the selection happened to be on, and the node inheriting it is already
    /// wherever it belongs in the order. Raising here would claim the user had focused
    /// something they never touched.
    pub(super) fn redirect_selection(&mut self, container: Option<NodeKey>) {
        self.selected = container;
    }

    /// The selection survives a command that only reshaped the tree around it.
    ///
    /// `split` and `layout` on a selected container leave it selected; the node is the same
    /// node and its place in the order is the one it already had.
    pub(super) fn keep_selected(&mut self, container: NodeKey) {
        self.selected = Some(container);
    }

    /// The focused leaf is going away; point at whatever takes its place.
    ///
    /// The counterpart of [`Self::redirect_selection`], and the same reasoning: a
    /// normalization removing a node is not the user focusing another one, so the order says
    /// what it said. Callers that mean "focus this" want [`Self::focus`].
    pub(super) fn redirect_focused_leaf(&mut self, leaf: Option<NodeKey>) {
        self.leaf = leaf;
    }

    /// Replace a focus-order entry while preserving its rank.
    ///
    /// Normal moves keep the same keys in the global tree and need no restoration. This is for
    /// structural replacement during normalization, where one materialized node takes another
    /// node's semantic place without implying a user focus change.
    pub(super) fn restore_at(&mut self, rank: usize, keys: impl IntoIterator<Item = NodeKey>) {
        let mut at = rank.min(self.order.len());
        for key in keys {
            self.order.retain(|entry| *entry != key);
            at = at.min(self.order.len());
            self.order.insert(at, key);
            at = (at + 1).min(self.order.len());
        }
    }

    /// Drop everything the tree no longer holds.
    pub(super) fn prune<W: LayoutElement>(&mut self, nodes: &NodeStore<W>) {
        self.order.retain(|key| nodes.contains_key(*key));
        if self.leaf.is_some_and(|key| !nodes.contains_key(key)) {
            self.leaf = None;
        }
        if self.selected.is_some_and(|key| !nodes.contains_key(key)) {
            self.selected = None;
        }
    }

    fn raise(&mut self, chain: &[NodeKey]) {
        let Some((node, ancestors)) = chain.split_first() else {
            return;
        };
        for ancestor in ancestors {
            self.order.retain(|entry| entry != ancestor);
            self.order.insert(0, *ancestor);
        }
        self.order.retain(|entry| entry != node);
        self.order.insert(0, *node);
    }
}
