//! Everything the seat knows about focus, and the only place it can be changed.
//!
//! The fields are private to this module on purpose. Focus used to be three loose values on
//! the tree, assigned from some thirty places, and only one of those places also updated the
//! order — so the order was right for the commands whose authors remembered it and quietly
//! wrong for the rest. The divergences that came of it all looked different: a window focused
//! but not shown, a tab that would not change, a `focus next` landing one window off. They
//! were one omission, made repeatedly, in code that had no way to notice.
//!
//! sway does not have this problem because it has no place to make the mistake:
//! `seat_set_focus` is a function, the stack is behind it, and there is nothing else to
//! assign. This is that shape.

use slotmap::SlotMap;

use super::{LayoutElement, NodeData, NodeKey};

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

    /// sway's `seat_set_focus`: the node becomes the most recent, and its ancestry with it.
    ///
    /// `chain` is the node first and its ancestors after, which is the order sway adds them
    /// in — a container sits ahead of its siblings exactly when something inside it was
    /// focused more recently than anything inside them.
    ///
    /// What is raised is what was *focused*. Focusing a container raises the container, not
    /// a window underneath it: the windows in there keep the order they had, and a later
    /// descent still knows which of them was in front.
    pub(super) fn focus(&mut self, chain: &[NodeKey], leaf: Option<NodeKey>) {
        self.raise(chain);
        self.leaf = leaf;
        self.selected = None;
    }

    /// The same, for a container `focus parent` left selected.
    pub(super) fn select(&mut self, chain: &[NodeKey], container: NodeKey, leaf: Option<NodeKey>) {
        self.raise(chain);
        self.leaf = leaf;
        self.selected = Some(container);
    }

    /// Record that focus is here, without saying anything about the selection.
    ///
    /// The difference from [`Self::focus`] is what a command means. `focus <somewhere>` is a
    /// new focus and drops the selection `focus parent` had made. Rebuilding the order after
    /// tree surgery is not a new focus — the same node still holds it, and the container the
    /// user had selected is still selected. Collapsing the two dropped the selection every
    /// time a `layout` command reshaped the tree under it.
    pub(super) fn touch(&mut self, chain: &[NodeKey], leaf: Option<NodeKey>) {
        self.raise(chain);
        self.leaf = leaf;
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

    /// A node took another's place in the tree, and takes its place in the order too.
    pub(super) fn replace(&mut self, old: NodeKey, new: NodeKey) {
        if let Some(slot) = self.order.iter_mut().find(|key| **key == old) {
            *slot = new;
        }
        if self.leaf == Some(old) {
            self.leaf = Some(new);
        }
        if self.selected == Some(old) {
            self.selected = Some(new);
        }
    }

    /// Take in nodes arriving from outside the tree, keeping the sequence they carried.
    ///
    /// Nothing is raised: arriving is not being focused. Nor is anything restored — a subtree
    /// that left went to a different tree with a different arena, so the keys it had are gone
    /// and its standing in this order with them. sway has no such loss because it has no such
    /// journey: floating moves a container between two lists of one workspace and the node
    /// stays the node. Until tiri's two trees are one, a floated window comes back as though
    /// nobody had focused it.
    pub(super) fn restore_at(&mut self, rank: usize, keys: impl IntoIterator<Item = NodeKey>) {
        let mut at = rank.min(self.order.len());
        for key in keys {
            if self.order.contains(&key) {
                continue;
            }
            self.order.insert(at, key);
            at = (at + 1).min(self.order.len());
        }
    }

    /// Drop everything the tree no longer holds.
    pub(super) fn prune<W: LayoutElement>(&mut self, nodes: &SlotMap<NodeKey, NodeData<W>>) {
        self.order.retain(|key| nodes.contains_key(*key));
        if self.leaf.is_some_and(|key| !nodes.contains_key(key)) {
            self.leaf = None;
        }
        if self.selected.is_some_and(|key| !nodes.contains_key(key)) {
            self.selected = None;
        }
    }

    fn raise(&mut self, chain: &[NodeKey]) {
        for node in chain.iter().rev() {
            self.order.retain(|entry| entry != node);
            self.order.insert(0, *node);
        }
    }
}
