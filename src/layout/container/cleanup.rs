//! Tree normalization.
//!
//! sway has no general tidy-up pass. It has two named operations, run by the commands that
//! need them and by no others, and which of them runs where is behaviour rather than
//! housekeeping: `container_reap_empty` after anything that takes a node out of the tree,
//! `workspace_squash` only after a directional move. A container left holding one child by
//! a `close` therefore stays, and the identical shape reached by a `move` does not.

use super::ContainerArena;
use super::ContainerData;
use super::Layout;
use super::LayoutElement;
use super::NodeData;
use super::NodeKey;

impl<W: LayoutElement> ContainerArena<W> {
    /// sway's `container_reap_empty`: destroy a container the command has emptied, and any
    /// ancestor it empties in turn.
    ///
    /// Only emptiness — this is not the place where redundant nesting goes, which is why a
    /// `close` leaves the container it emptied down to one child standing.
    ///
    /// A branch's root is where the walk stops. It has no parent to be detached from, and an
    /// empty workspace is an empty workspace rather than a container to be got rid of. An
    /// emptied floating root is not reaped either: the floating side owns that list, and it
    /// drops the group when the last window leaves it.
    pub(super) fn reap_empty(&mut self, key: NodeKey) {
        let mut current = Some(key);
        while let Some(container_key) = current {
            if container_key == self.root
                || (self.branch_root(container_key) == container_key
                    && self.branch_is_addressable(container_key))
            {
                return;
            }
            let Some(container) = self.get_container(container_key) else {
                return;
            };
            if container.child_count() > 0 {
                return;
            }

            let parent_key = self.parent_of(container_key);
            let inherits_fullscreen_selection = self.selected_key() == Some(container_key)
                && self.fullscreen_key.is_some_and(|fullscreen| {
                    container_key == fullscreen || self.is_descendant(container_key, fullscreen)
                });
            if let Some(parent_key) = parent_key {
                if inherits_fullscreen_selection {
                    // Fullscreen destruction is the measured exception: selection walks up
                    // the emptied fullscreen chain and can finish on the workspace itself.
                    // `reconcile_focus_after_change` preserves that workspace selection while
                    // replacing its inactive leaf.
                    self.seat.redirect_selection(Some(parent_key));
                    self.seat.unregister(container_key);
                } else {
                    // Ordinary destruction does not make a selected node's parent inherit
                    // focus. The shared unregister path also preserves Sway's raw focus-order
                    // side effect when this container was not selected.
                    self.unregister_unfocused_node(container_key, parent_key);
                }
            }
            self.detach_child(container_key);
            self.remove_node_from_store(container_key);
            current = parent_key;
        }
    }

    /// sway's `workspace_squash`: drop nesting that says the same thing twice, everywhere in
    /// the workspace.
    ///
    /// Only a directional move runs this. Other commands leave the nesting they produce
    /// alone, and the difference is measurable: a `split` builds a container holding one
    /// window and it stays, a `close` down to one child keeps both levels, and the same
    /// shape reached by a `move` collapses.
    pub(super) fn squash_branch(&mut self, branch_root: NodeKey) {
        self.squash_children(branch_root);
    }

    /// Squash every child of `key`, walking the list as it changes underneath: a squashed
    /// child is replaced by however many grandchildren it had.
    fn squash_children(&mut self, key: NodeKey) {
        let mut idx = 0;
        while let Some(child_key) = self.get_container(key).and_then(|c| c.child_key(idx)) {
            idx += self.squash_container(child_key) + 1;
        }
    }

    /// sway's `container_squash`. Returns how many extra slots the container now occupies in
    /// its parent's child list — zero unless it was spliced away.
    fn squash_container(&mut self, key: NodeKey) -> usize {
        let child_key = match self.get_container(key) {
            Some(container) if container.child_count() == 1 => container.child_key(0),
            // A leaf, or a container with a real arrangement of its own to look inside.
            _ => None,
        };
        let Some(child_key) = child_key.filter(|child| self.is_squashable(key, *child)) else {
            if matches!(self.get_node(key), Some(NodeData::Container(_))) {
                self.squash_children(key);
            }
            return 0;
        };

        self.splice_squashed_pair(key, child_key)
    }

    /// Destroy a redundant pair, the way sway destroys it.
    ///
    /// sway takes the child's children off the front one at a time and puts each back at the
    /// container's own index, so the last one taken ends up first: the contents land in the
    /// parent **reversed**. That is the recorded behaviour, and it is what the tree does.
    ///
    /// No share crosses over: each arriving child keeps the fraction it had inside the pair,
    /// the pair's own is dropped with it, and the list is left for the end-of-command resolve
    /// to normalize. That only comes out right because the other half is in place — `cmd_move`
    /// invalidates the fraction of what it moved, and the resolve fills an unset one with the
    /// average of the rest. Dividing the pair's share among its children instead, which is
    /// what this did while only half the rule existed, is what left the workspace lopsided
    /// where sway had levelled it.
    fn splice_squashed_pair(&mut self, con_key: NodeKey, child_key: NodeKey) -> usize {
        let Some(parent_key) = self.parent_of(con_key) else {
            return 0;
        };
        let Some(idx) = self.child_index(parent_key, con_key) else {
            return 0;
        };
        let Some(child) = self.get_container(child_key) else {
            return 0;
        };

        let taken = child.children.clone();
        if taken.is_empty() {
            return 0;
        }

        if let Some(parent) = self.get_container_mut(parent_key) {
            parent
                .children
                .splice(idx..=idx, taken.iter().rev().copied());

            // No focus order to splice. The grandchildren are already in the seat's order
            // wherever they belong in it, and destroying the two levels above them changes
            // who their parent is and nothing else — which is the whole of what the order is
            // read for.
        }

        for grandchild in &taken {
            self.set_parent(*grandchild, Some(parent_key));
        }

        // Whatever pointed at either level now points at the grandchild that was focused
        // inside them, which is where the focus was all along — and the seat's order already
        // says which one that is, without the pair having to be asked.
        let inherits = self
            .seat
            .order()
            .iter()
            .copied()
            .find(|key| taken.contains(key))
            .or_else(|| taken.first().copied());
        if matches!(self.selected_key(), Some(k) if k == con_key || k == child_key) {
            self.seat.redirect_selection(inherits);
        }
        if matches!(self.focused_key(), Some(k) if k == con_key || k == child_key) {
            self.seat.redirect_focused_leaf(inherits);
        }

        for key in [child_key, con_key] {
            self.unregister_unfocused_node(key, parent_key);
            self.remove_node_from_store(key);
        }
        self.prune_focus_order();

        taken.len() - 1
    }

    /// Whether `con` and its only `child` are a redundant pair: two splits crossing each
    /// other where the grandparent already lays its children out the child's way, so the
    /// container in the middle adds an orientation nothing reads.
    fn is_squashable(&self, con_key: NodeKey, child_key: NodeKey) -> bool {
        let Some(con_layout) = self
            .get_container(con_key)
            .map(|container| container.layout())
        else {
            return false;
        };
        let Some(child_layout) = self
            .get_container(child_key)
            .map(|container| container.layout())
        else {
            return false;
        };
        let Some(grandparent_layout) = self
            .parent_of(con_key)
            .and_then(|key| self.get_container(key))
            .map(|container| container.layout())
        else {
            return false;
        };

        matches!(con_layout, Layout::SplitH | Layout::SplitV)
            && matches!(child_layout, Layout::SplitH | Layout::SplitV)
            && !con_layout.is_parallel_to_layout(child_layout)
            && grandparent_layout.is_parallel_to_layout(child_layout)
    }

    pub(super) fn ensure_container_at_path(
        &mut self,
        branch_root: NodeKey,
        path: &[usize],
        layout: Layout,
    ) -> Option<NodeKey> {
        if path.is_empty() {
            return Some(branch_root);
        }

        let key = self.node_at_branch_path(branch_root, path)?;
        if matches!(self.get_node(key), Some(NodeData::Container(_))) {
            return Some(key);
        }

        let parent_path = &path[..path.len() - 1];
        let parent_key = if parent_path.is_empty() {
            branch_root
        } else {
            self.node_at_branch_path(branch_root, parent_path)?
        };

        let mut container = ContainerData::new(layout);
        container.mark_user_created();
        self.wrap_child_in_new_container(parent_key, key, container)
    }
}
