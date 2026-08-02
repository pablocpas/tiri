//! Tree normalization: collapsing redundant containers after mutations.

use super::ContainerData;
use super::ContainerTree;
use super::Layout;
use super::LayoutElement;
use super::NodeData;
use super::NodeKey;

impl<W: LayoutElement> ContainerTree<W> {
    /// Normalize the tree after a mutation, walking from `key` towards the root: drop empty
    /// containers, dissolve single-child wrappers and squash redundant nested splits.
    pub(super) fn cleanup_containers(&mut self, mut key: Option<NodeKey>) {
        while let Some(container_key) = key {
            let parent_key = self.parent_of(container_key);
            // Flattening lifts a subtree a level, so carry on from what was promoted: it
            // may itself be redundant now that it sits somewhere else.
            if let Some(promoted) = self.flatten_redundant_split(container_key, parent_key) {
                key = Some(promoted);
                continue;
            }
            self.cleanup_one_container(container_key, parent_key);
            key = parent_key;
        }

        self.remove_root_if_empty();
        self.collapse_singleton_root_chain();
    }

    /// Dissolve a lone wrapper that only re-states the orientation its grandparent already
    /// has, lifting its grandchildren into its place.
    ///
    /// This is i3's `tree_flatten`, and the shape it targets is narrow on purpose: a
    /// container holding exactly one child, that child a *split* whose layout matches the
    /// grandparent's. `splith > [ splitv > [ splith > w ], … ]` is three ways of saying the
    /// same arrangement, so sway keeps only the outer one.
    ///
    /// Everything just outside that shape is a state sway does keep, and each was measured:
    /// a single-child split holding a window (`split` builds one and it survives), a tabbed
    /// or stacked child (nesting two tab bars is meaningful), and a child whose orientation
    /// differs from the grandparent's (it is what makes the layout two-dimensional).
    fn flatten_redundant_split(
        &mut self,
        container_key: NodeKey,
        parent_key: Option<NodeKey>,
    ) -> Option<NodeKey> {
        let parent_key = parent_key?;
        let container = self.get_container(container_key)?;
        if container.child_count() != 1 {
            return None;
        }
        let container_layout = container.layout();
        let child_key = container.child_key(0)?;
        let child_layout = self.get_container(child_key)?.layout();
        if !matches!(child_layout, Layout::SplitH | Layout::SplitV)
            || child_layout == container_layout
        {
            return None;
        }
        if self.get_container(parent_key).map(|parent| parent.layout()) != Some(child_layout) {
            return None;
        }

        let replaced = self
            .get_container_mut(parent_key)
            .is_some_and(|parent| parent.replace_child_preserving_focus(container_key, child_key));
        if !replaced {
            return None;
        }
        self.set_parent(child_key, Some(parent_key));
        self.nodes.remove(container_key);
        self.parents.remove(container_key);
        if self.selected_key == Some(container_key) {
            self.selected_key = Some(child_key);
        }
        if self.focused_key == Some(container_key) {
            self.focused_key = Some(child_key);
        }

        // The child now sits where the wrapper did, saying what its parent already says, so
        // it goes too and its children take the slot. i3 splices rather than promotes here,
        // and the difference is visible: promoting would leave one more level than sway has.
        let Some(child) = self.get_container(child_key) else {
            return Some(child_key);
        };
        let grandchildren = child.children.clone();
        let focus_stack = child.focus_stack.clone();
        let percents = child.child_percents_slice().to_vec();
        self.squash_container_into_parent(
            child_key,
            parent_key,
            &grandchildren,
            &focus_stack,
            &percents,
        );
        Some(parent_key)
    }

    /// Apply at most one normalization step to `container_key`.
    fn cleanup_one_container(&mut self, container_key: NodeKey, parent_key: Option<NodeKey>) {
        let Some(container) = self.get_container(container_key) else {
            return;
        };

        let container_layout = container.layout();
        let container_children = container.children.clone();
        let container_focus_stack = container.focus_stack.clone();
        let container_child_percents = container.child_percents_slice().to_vec();
        let container_is_user_made = container.is_user_container();
        let child_count = container_children.len();

        let parent_layout =
            parent_key.and_then(|key| self.get_container(key).map(|parent| parent.layout()));

        let single_child_key = container_children.first().copied();
        // A container with a parent is never dissolved here, whatever its layout and however
        // few children it has left. Measured: a `split` builds one holding a single window
        // and it survives, a `close` that empties one down to a single child leaves it
        // alone, and so does a move elsewhere in the tree. The only lone wrapper that goes
        // is a *root* one holding a window, because a workspace whose only child is a window
        // has no container in sway either.
        let can_replace_with_child = parent_key.is_none()
            && single_child_key.is_some_and(|child_key| {
                matches!(self.get_node(child_key), Some(NodeData::Leaf(_)))
            });

        if child_count == 0 {
            self.remove_empty_container(container_key, parent_key, container_layout);
        } else if child_count == 1 && can_replace_with_child {
            let Some(child_key) = single_child_key else {
                return;
            };
            self.replace_container_with_child(container_key, parent_key, child_key);
        } else if child_count > 1
            && !container_is_user_made
            && parent_layout
                .map(|layout| Self::layouts_squashable(layout, container_layout))
                .unwrap_or(false)
        {
            let Some(parent_key) = parent_key else {
                return;
            };
            self.squash_container_into_parent(
                container_key,
                parent_key,
                &container_children,
                &container_focus_stack,
                &container_child_percents,
            );
        }
    }

    /// Remove a container with no children. Removing the root remembers its layout as the
    /// workspace's pending layout, matching i3's workspace_layout persistence.
    fn remove_empty_container(
        &mut self,
        container_key: NodeKey,
        parent_key: Option<NodeKey>,
        container_layout: Layout,
    ) {
        if let Some(parent_key) = parent_key {
            let Some(parent_idx) = self.child_index(parent_key, container_key) else {
                return;
            };
            if let Some(parent) = self.get_container_mut(parent_key) {
                parent.remove_child(parent_idx);
            }
            self.set_parent(container_key, None);
            self.remove_node_recursive(container_key);
        } else {
            self.pending_layout = Some(container_layout);
            self.pending_layout_wrap_on_split = false;
            self.remove_node_recursive(container_key);
            self.root = None;
        }
    }

    /// Dissolve a single-child wrapper by promoting its only child into its place.
    fn replace_container_with_child(
        &mut self,
        container_key: NodeKey,
        parent_key: Option<NodeKey>,
        child_key: NodeKey,
    ) {
        if let Some(parent_key) = parent_key {
            if self.child_index(parent_key, container_key).is_none() {
                return;
            }
            if let Some(parent) = self.get_container_mut(parent_key) {
                parent.replace_child_preserving_focus(container_key, child_key);
            }
            self.set_parent(child_key, Some(parent_key));
        } else {
            if self.selected_key == Some(container_key) {
                self.selected_key = Some(child_key);
            }
            if self.focused_key == Some(container_key) {
                self.focused_key = Some(child_key);
            }
            self.set_parent(child_key, None);
            self.root = Some(child_key);
        }
        self.nodes.remove(container_key);
        self.parents.remove(container_key);
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

    /// Drop the root container once it has no children left.
    fn remove_root_if_empty(&mut self) {
        let Some(root_key) = self.root else {
            return;
        };
        let Some(container) = self.get_container(root_key) else {
            return;
        };
        if container.children.is_empty() {
            self.pending_layout = None;
            self.pending_layout_wrap_on_split = false;
            self.remove_node_recursive(root_key);
            self.root = None;
        }
    }

    /// Dissolve an implicit root that holds a single window.
    ///
    /// A workspace whose only child is a window has no container in sway either, so the
    /// wrapper is representation and goes. A workspace whose only child is a *container* is
    /// a different state and sway keeps both levels — collapsing it would promote that
    /// container into the workspace and hand it the workspace's layout, which is how
    /// closing tabs down to the last one used to leave the workspace itself tabbed.
    fn collapse_singleton_root_chain(&mut self) {
        while let Some(root_key) = self.root {
            let Some(root) = self.get_container(root_key) else {
                break;
            };
            if root.child_count() != 1 {
                break;
            }

            let Some(child_key) = root.child_key(0) else {
                break;
            };
            if self.get_container(child_key).is_some() {
                break;
            }
            if self.selected_key == Some(root_key) {
                self.selected_key = Some(child_key);
            }
            if self.focused_key == Some(root_key) {
                self.focused_key = Some(child_key);
            }

            self.set_parent(child_key, None);
            self.nodes.remove(root_key);
            self.parents.remove(root_key);
            self.root = Some(child_key);
        }
    }

    pub(super) fn ensure_root_container(&mut self) -> NodeKey {
        if self.root.is_none() {
            let explicit_layout = self.pending_layout.is_some();
            let layout = self.layout_for_workspace_container();
            self.pending_layout_wrap_on_split = false;
            let mut container = ContainerData::new(layout);
            if explicit_layout {
                container.mark_user_created();
            }
            let container_key = self.insert_node(NodeData::Container(container));
            self.set_parent(container_key, None);
            self.root = Some(container_key);
            self.focused_key = None;
            return container_key;
        }

        let root_key = self.expect_root();
        let needs_conversion = matches!(self.get_node(root_key), Some(NodeData::Leaf(_)));

        if needs_conversion {
            let old_root_key = self.take_root();
            let mut container = ContainerData::new(Layout::SplitH);
            container.add_child(old_root_key);
            let container_key = self.insert_node(NodeData::Container(container));
            self.set_parent(old_root_key, Some(container_key));
            self.set_parent(container_key, None);
            self.root = Some(container_key);
            self.focus_node_key(old_root_key);
            container_key
        } else {
            root_key
        }
    }

    pub(super) fn ensure_container_at_path(
        &mut self,
        path: &[usize],
        layout: Layout,
    ) -> Option<NodeKey> {
        let root_key = self.root?;
        if path.is_empty() {
            if matches!(self.get_node(root_key), Some(NodeData::Container(_))) {
                return Some(root_key);
            }

            let mut container = ContainerData::new(layout);
            container.mark_user_created();
            container.add_child(root_key);
            let container_key = self.insert_node(NodeData::Container(container));
            self.set_parent(root_key, Some(container_key));
            self.set_parent(container_key, None);
            self.root = Some(container_key);
            return Some(container_key);
        }

        let key = self.get_node_key_at_path(path)?;
        if matches!(self.get_node(key), Some(NodeData::Container(_))) {
            return Some(key);
        }

        let parent_path = &path[..path.len() - 1];
        let parent_key = if parent_path.is_empty() {
            self.root?
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
