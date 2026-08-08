//! Tree normalization.
//!
//! sway has no general tidy-up pass. It has two named operations, run by the commands that
//! need them and by no others, and which of them runs where is behaviour rather than
//! housekeeping: `container_reap_empty` after anything that takes a node out of the tree,
//! `workspace_squash` only after a directional move. A container left holding one child by
//! a `close` therefore stays, and the identical shape reached by a `move` does not.

use super::ContainerData;
use super::ContainerTree;
use super::Layout;
use super::LayoutElement;
use super::NodeData;
use super::NodeKey;

impl<W: LayoutElement> ContainerTree<W> {
    /// sway's `container_reap_empty`: destroy a container the command has emptied, and any
    /// ancestor it empties in turn.
    ///
    /// Only emptiness — this is not the place where redundant nesting goes, which is why a
    /// `close` leaves the container it emptied down to one child standing.
    ///
    /// The workspace is where the walk stops. It has no parent to be detached from, and an
    /// empty one is an empty workspace rather than a container to be got rid of.
    pub(super) fn reap_empty(&mut self, key: NodeKey) {
        let mut current = Some(key);
        while let Some(container_key) = current {
            if container_key == self.root {
                return;
            }
            let Some(container) = self.get_container(container_key) else {
                return;
            };
            if container.child_count() > 0 {
                return;
            }

            let parent_key = self.parent_of(container_key);
            self.detach_child(container_key);
            self.nodes.remove(container_key);
            self.parents.remove(container_key);
            if self.selected_key == Some(container_key) {
                self.selected_key = parent_key;
            }
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
    pub(super) fn squash_workspace(&mut self) {
        self.squash_children(self.root);
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
        let child_focus = child.focus_stack.clone();
        let child_fractions = child.fractions.clone();
        if taken.is_empty() {
            return 0;
        }

        if let Some(parent) = self.get_container_mut(parent_key) {
            let old_len = parent.children.len();
            parent
                .children
                .splice(idx..=idx, taken.iter().rev().copied());

            let fractions_were_consistent =
                parent
                    .fractions
                    .splice_child(idx, old_len, &child_fractions);

            let mut focus = Vec::with_capacity(parent.focus_stack.len() + taken.len() - 1);
            for key in std::mem::take(&mut parent.focus_stack) {
                if key == con_key {
                    focus.extend(child_focus.iter().filter(|key| taken.contains(key)));
                } else {
                    focus.push(key);
                }
            }
            parent.focus_stack = focus;

            if !fractions_were_consistent {
                parent.fractions.resize_unset(parent.children.len());
                parent.recalculate_percentages();
            }
            parent.ensure_focus_stack();
        }

        for grandchild in &taken {
            self.set_parent(*grandchild, Some(parent_key));
        }

        // Whatever pointed at either level now points at the grandchild that was focused
        // inside them, which is where the focus was all along.
        let inherits = child_focus
            .iter()
            .copied()
            .find(|key| taken.contains(key))
            .or_else(|| taken.first().copied());
        for key in [&mut self.selected_key, &mut self.focused_key] {
            if *key == Some(con_key) || *key == Some(child_key) {
                *key = inherits;
            }
        }

        for key in [child_key, con_key] {
            self.nodes.remove(key);
            self.parents.remove(key);
        }

        taken.len() - 1
    }

    /// Whether `con` and its only `child` are a redundant pair: two splits crossing each
    /// other where the grandparent already lays its children out the child's way, so the
    /// container in the middle adds an orientation nothing reads.
    fn is_squashable(&self, con_key: NodeKey, child_key: NodeKey) -> bool {
        let Some(con_layout) = self.get_container(con_key).map(ContainerData::layout) else {
            return false;
        };
        let Some(child_layout) = self.get_container(child_key).map(ContainerData::layout) else {
            return false;
        };
        let Some(grandparent_layout) = self
            .parent_of(con_key)
            .and_then(|key| self.get_container(key))
            .map(ContainerData::layout)
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
        path: &[usize],
        layout: Layout,
    ) -> Option<NodeKey> {
        if path.is_empty() {
            return Some(self.root);
        }

        let key = self.get_node_key_at_path(path)?;
        if matches!(self.get_node(key), Some(NodeData::Container(_))) {
            return Some(key);
        }

        let parent_path = &path[..path.len() - 1];
        let parent_key = if parent_path.is_empty() {
            self.root
        } else {
            self.get_node_key_at_path(parent_path)?
        };

        let mut container = ContainerData::new(layout);
        container.mark_user_created();
        self.wrap_child_in_new_container(parent_key, key, container)
    }
}
