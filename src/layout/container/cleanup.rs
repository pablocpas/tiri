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

        // The grandchildren take the slot both levels were holding. Done as two splices so
        // the size shares are redistributed by the same arithmetic as everywhere else: the
        // grandchildren fill the child, then the child's contents fill the container's slot.
        let extra = self
            .get_container(child_key)
            .map_or(0, |child| child.child_count())
            .saturating_sub(1);
        self.splice_container_into_parent(child_key);
        self.splice_container_into_parent(key);
        extra
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

    /// Replace a container by its children in its parent's child list.
    fn splice_container_into_parent(&mut self, container_key: NodeKey) {
        let Some(parent_key) = self.parent_of(container_key) else {
            return;
        };
        let Some(container) = self.get_container(container_key) else {
            return;
        };
        let children = container.children.clone();
        let focus_stack = container.focus_stack.clone();
        let percents = container.child_percents_slice().to_vec();
        self.squash_container_into_parent(
            container_key,
            parent_key,
            &children,
            &focus_stack,
            &percents,
        );
    }

    /// Splice a container's children into its parent in place of the container itself,
    /// merging focus stacks and scaling child percents into the replaced share.
    fn squash_container_into_parent(
        &mut self,
        container_key: NodeKey,
        parent_key: NodeKey,
        container_children: &[NodeKey],
        container_focus_stack: &[NodeKey],
        container_child_percents: &[f64],
    ) {
        let Some(parent_idx) = self.child_index(parent_key, container_key) else {
            return;
        };
        let Some(parent) = self.get_container(parent_key) else {
            return;
        };
        let parent_children = parent.children.clone();
        let parent_focus = parent.focus_stack.clone();
        let parent_percents = parent.child_percents_slice().to_vec();

        let mut new_children =
            Vec::with_capacity(parent_children.len().saturating_sub(1) + container_children.len());
        new_children.extend_from_slice(&parent_children[..parent_idx]);
        new_children.extend_from_slice(container_children);
        new_children.extend_from_slice(&parent_children[parent_idx + 1..]);

        let mut new_focus =
            Vec::with_capacity(parent_focus.len().saturating_sub(1) + container_focus_stack.len());
        for key in parent_focus {
            if key == container_key {
                for child in container_focus_stack {
                    if container_children.contains(child) && !new_focus.contains(child) {
                        new_focus.push(*child);
                    }
                }
            } else if !new_focus.contains(&key) {
                new_focus.push(key);
            }
        }
        for child in container_children {
            if !new_focus.contains(child) {
                new_focus.push(*child);
            }
        }

        let new_percents = squashed_child_percents(
            &parent_percents,
            parent_children.len(),
            parent_idx,
            container_child_percents,
            container_children.len(),
        );

        if let Some(parent) = self.get_container_mut(parent_key) {
            parent.children = new_children;
            parent.focus_stack = new_focus;
            if new_percents.len() == parent.children.len() {
                parent.child_percents = new_percents;
                parent.normalize_child_percents();
            } else {
                parent.recalculate_percentages();
            }
            parent.ensure_focus_stack();
        }

        for child_key in container_children {
            self.set_parent(*child_key, Some(parent_key));
        }

        let redirected = container_focus_stack
            .iter()
            .copied()
            .find(|child| container_children.contains(child))
            .or_else(|| container_children.first().copied())
            .or(Some(parent_key));
        if self.selected_key == Some(container_key) {
            self.selected_key = redirected;
        }
        if self.focused_key == Some(container_key) {
            self.focused_key = redirected;
        }

        self.nodes.remove(container_key);
        self.parents.remove(container_key);
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
        let child_idx = *path.last().unwrap();

        let mut container = ContainerData::new(layout);
        container.mark_user_created();
        container.add_child(key);
        let container_key = self.insert_node(NodeData::Container(container));
        self.set_parent(key, Some(container_key));

        if let Some(parent) = self.get_container_mut(parent_key) {
            parent.children[child_idx] = container_key;
            if let Some(pos) = parent.focus_stack.iter().position(|k| *k == key) {
                parent.focus_stack[pos] = container_key;
            } else if !parent.focus_stack.contains(&container_key) {
                parent.focus_stack.push(container_key);
            }
            parent.ensure_focus_stack();
        }

        self.set_parent(container_key, Some(parent_key));
        Some(container_key)
    }
}

/// Distribute the parent share previously occupied by a squashed container across its
/// children, proportionally to their own percents. Returns an empty vec when the parent
/// percents are inconsistent, signalling the caller to recalculate from scratch.
fn squashed_child_percents(
    parent_percents: &[f64],
    parent_child_count: usize,
    parent_idx: usize,
    container_percents: &[f64],
    container_child_count: usize,
) -> Vec<f64> {
    let mut new_percents = Vec::new();
    if parent_percents.len() != parent_child_count {
        return new_percents;
    }

    let replaced_share = parent_percents[parent_idx];
    new_percents.extend_from_slice(&parent_percents[..parent_idx]);

    if container_child_count > 0 {
        let sum: f64 = container_percents.iter().copied().sum();
        if container_percents.len() == container_child_count && sum > f64::EPSILON {
            for percent in container_percents {
                new_percents.push(replaced_share * (*percent / sum));
            }
        } else {
            let value = replaced_share / container_child_count as f64;
            new_percents.resize(new_percents.len() + container_child_count, value);
        }
    }

    new_percents.extend_from_slice(&parent_percents[parent_idx + 1..]);
    new_percents
}
