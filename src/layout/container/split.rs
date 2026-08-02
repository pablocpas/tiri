//! Split, layout-toggle and root-wrapping commands.

use super::ContainerData;
use super::ContainerTree;
use super::Layout;
use super::LayoutElement;
use super::NodeData;
use super::NodeKey;
use super::RootPolicy;
use super::TreeCommandTarget;

impl<W: LayoutElement> ContainerTree<W> {
    pub(super) fn set_layout_for_container_target(
        &mut self,
        container_key: NodeKey,
        layout: Layout,
        root_policy: RootPolicy,
    ) -> bool {
        if !matches!(self.get_node(container_key), Some(NodeData::Container(_))) {
            return false;
        }

        if container_key == self.root {
            if root_policy == RootPolicy::MaterialContainer {
                return false;
            }
            // The workspace itself is selected, so it is what takes the layout.
            return self.set_root_container_layout(layout);
        }

        self.set_layout_of_parent_of(container_key, layout, root_policy)
    }

    pub(in crate::layout) fn set_root_container_layout(&mut self, layout: Layout) -> bool {
        let root_key = self.root;
        let Some(root) = self.get_container_mut(root_key) else {
            return false;
        };
        // Not `set_layout_explicit`: the workspace does not need a user-created bit to be
        // addressable, and setting it would make it read as an ordinary container to the
        // rules that ask.
        root.set_layout(layout);
        true
    }

    pub(in crate::layout) fn set_layout_for_target(
        &mut self,
        layout: Layout,
        target: TreeCommandTarget,
        root_policy: RootPolicy,
    ) -> bool {
        match target {
            TreeCommandTarget::Workspace => false,
            TreeCommandTarget::Container(key) => {
                self.set_layout_for_container_target(key, layout, root_policy)
            }
            TreeCommandTarget::Leaf(key) => {
                if !matches!(self.get_node(key), Some(NodeData::Leaf(_))) {
                    return false;
                }
                self.focus_node_key(key);
                self.set_focused_layout_with_policy(layout, root_policy)
            }
        }
    }

    pub(super) fn layout_container_key_for_target(
        &self,
        target: TreeCommandTarget,
        root_policy: RootPolicy,
    ) -> Option<NodeKey> {
        match target {
            TreeCommandTarget::Workspace => None,
            TreeCommandTarget::Container(key) => {
                if !matches!(self.get_node(key), Some(NodeData::Container(_))) {
                    return None;
                }
                if key == self.root && root_policy == RootPolicy::MaterialContainer {
                    return None;
                }
                if key == self.root {
                    Some(key)
                } else {
                    self.parent_of(key)
                }
            }
            TreeCommandTarget::Leaf(key) => {
                if !matches!(self.get_node(key), Some(NodeData::Leaf(_))) {
                    return None;
                }
                // A root leaf has no owning container, so the root itself is the target.
                self.parent_of(key).or(Some(self.root))
            }
        }
    }

    /// Layout of the container holding `key`.
    pub(in crate::layout) fn parent_layout(&self, key: NodeKey) -> Option<Layout> {
        let parent_key = self.parent_of(key)?;
        self.get_container(parent_key).map(|c| c.layout())
    }

    /// If `key`'s parent is an explicit split holding only `key`, that split's layout.
    pub(in crate::layout) fn single_child_split_layout(&self, key: NodeKey) -> Option<Layout> {
        let parent_key = self.parent_of(key)?;

        let container = self.get_container(parent_key)?;
        if container.child_count() != 1 || !container.is_user_container() {
            return None;
        }

        match container.layout() {
            Layout::SplitH | Layout::SplitV => Some(container.layout()),
            _ => None,
        }
    }

    /// Move the workspace's children under a new container, giving the wrapper and the
    /// workspace a layout each.
    ///
    /// `split` and `layout` on a workspace are the same surgery with the two layouts
    /// swapped, which is the whole of their difference. Measured against sway 1.11:
    ///
    /// - `split X` keeps the old layout on the wrapper and puts X on the workspace, and
    ///   builds the container unconditionally — one child or several, orientation changed
    ///   or not;
    /// - `layout X` puts X on the wrapper and leaves the workspace as it was.
    ///
    /// It is also how a subtree is grouped before being floated as a whole.
    ///
    /// Returns the wrapper it built, for the callers that need to say something about it —
    /// `split` leaves it selected, the others only care that it worked.
    pub(in crate::layout) fn wrap_workspace_children(
        &mut self,
        wrapper_layout: Layout,
        root_layout: Layout,
    ) -> Option<NodeKey> {
        let root_key = self.root;

        let (old_children, old_focus_stack, old_child_percents, root_geometry) = {
            let root = self.get_container_mut(root_key)?;
            if root.children.is_empty() {
                return None;
            }

            (
                std::mem::take(&mut root.children),
                std::mem::take(&mut root.focus_stack),
                std::mem::take(&mut root.child_percents),
                root.geometry,
            )
        };

        let mut wrapper = ContainerData::new(wrapper_layout);
        wrapper.children = old_children;
        wrapper.focus_stack = old_focus_stack;
        wrapper.child_percents = old_child_percents;
        wrapper.geometry = root_geometry;
        // Only a wrapper that holds a single node is worth protecting from cleanup: it is
        // saying something the workspace does not. One holding several is an ordinary
        // container, and if windows later leave it until one remains, it is as redundant as
        // any other — which is what sway does with it.
        if wrapper.child_count() == 1 {
            wrapper.mark_user_created();
        }
        wrapper.ensure_focus_stack();

        let wrapper_children = wrapper.children.clone();
        let wrapper_key = self.insert_node(NodeData::Container(wrapper));
        for child in wrapper_children {
            self.set_parent(child, Some(wrapper_key));
        }

        let root = self.get_container_mut(root_key)?;
        root.layout = root_layout;
        root.children.push(wrapper_key);
        root.focus_stack.push(wrapper_key);
        root.child_percents.push(1.0);
        root.ensure_focus_stack();
        self.set_parent(wrapper_key, Some(root_key));

        if let Some(focused_key) = self.focused_key {
            self.sync_container_focus_from_key(focused_key);
        }

        Some(wrapper_key)
    }

    /// i3's `tree_flatten`, at the workspace.
    ///
    /// A workspace whose only child is a split saying what the workspace already says has a
    /// level that means nothing, so that child's children move up into its place. It splices
    /// rather than promotes — promoting would hand the workspace's identity to a container
    /// that is not it.
    pub(in crate::layout) fn collapse_redundant_root_single_child_split(&mut self) -> bool {
        let mut changed = false;

        loop {
            let root_key = self.root;
            let Some(root_container) = self.get_container(root_key) else {
                break;
            };
            if root_container.child_count() != 1 {
                break;
            }

            let root_layout = root_container.layout();
            if !matches!(root_layout, Layout::SplitH | Layout::SplitV) {
                break;
            }

            let Some(child_key) = root_container.child_key(0) else {
                break;
            };
            let Some(child_container) = self.get_container(child_key) else {
                break;
            };
            if child_container.layout() != root_layout {
                break;
            }

            let grandchildren = child_container.children.clone();
            let focus_stack = child_container.focus_stack.clone();
            let percents = child_container.child_percents_slice().to_vec();

            let Some(root) = self.get_container_mut(root_key) else {
                break;
            };
            root.children = grandchildren.clone();
            root.focus_stack = focus_stack;
            root.child_percents = percents;
            root.ensure_focus_stack();
            for grandchild in grandchildren {
                self.set_parent(grandchild, Some(root_key));
            }

            if self.selected_key == Some(child_key) {
                self.selected_key = Some(root_key);
            }
            self.nodes.remove(child_key);
            self.parents.remove(child_key);
            changed = true;
        }

        changed
    }

    pub(super) fn layouts_squashable(parent: Layout, child: Layout) -> bool {
        // Only collapse truly redundant split levels. Tabbed/stacked containers
        // must keep their own node to preserve semantics.
        matches!(
            (parent, child),
            (Layout::SplitH, Layout::SplitH) | (Layout::SplitV, Layout::SplitV)
        )
    }

    pub(super) fn split_selected_container(
        &mut self,
        selected_key: NodeKey,
        layout: Layout,
        root_policy: RootPolicy,
    ) -> bool {
        if !matches!(self.get_node(selected_key), Some(NodeData::Container(_))) {
            return false;
        }

        if selected_key == self.root {
            return match root_policy {
                RootPolicy::ImplicitWorkspace => self.split_workspace_container(layout),
                RootPolicy::MaterialContainer => self.split_root_container(layout),
            };
        }

        let Some(parent_key) = self.parent_of(selected_key) else {
            return false;
        };

        let (parent_layout, parent_child_count) = match self.get_container(parent_key) {
            Some(container) => (container.layout(), container.child_count()),
            None => return false,
        };

        if parent_child_count == 1 && matches!(parent_layout, Layout::SplitH | Layout::SplitV) {
            if let Some(container) = self.get_container_mut(parent_key) {
                container.set_layout(layout);
            }
            return true;
        }

        let Some(child_idx) = self.child_index(parent_key, selected_key) else {
            return false;
        };

        if let Some(parent) = self.get_container_mut(parent_key) {
            parent.remove_child(child_idx);
        }
        self.set_parent(selected_key, None);

        let mut wrapper = ContainerData::new(layout);
        wrapper.mark_user_created();
        wrapper.add_child(selected_key);
        let wrapper_key = self.insert_node(NodeData::Container(wrapper));
        self.set_parent(selected_key, Some(wrapper_key));

        if let Some(parent) = self.get_container_mut(parent_key) {
            parent.insert_child(child_idx, wrapper_key);
        }
        self.set_parent(wrapper_key, Some(parent_key));

        // Keep command context on the originally selected container.
        self.selected_key = Some(selected_key);
        if let Some(focused_key) = self.focused_key {
            self.sync_container_focus_from_key(focused_key);
        } else if let Some(leaf_key) = self.leaf_under_key(selected_key) {
            self.focused_key = Some(leaf_key);
            self.sync_container_focus_from_key(leaf_key);
        }

        true
    }

    /// `split X` with the workspace itself selected.
    ///
    /// Measured against sway 1.11: this always builds a container. The workspace's children
    /// move under a wrapper keeping the old orientation and the workspace takes the new one —
    /// with a single child, and even when the orientation does not change. The wrapper is
    /// left selected, so a command straight after lands on it. An empty workspace has nothing
    /// to wrap and only takes the orientation.
    ///
    /// It is the counterpart of `layout X` on the workspace, which never wraps; the whole
    /// difference between the two commands is which of the two layouts goes where.
    pub(in crate::layout) fn split_workspace_container(&mut self, layout: Layout) -> bool {
        let previous = self.root_container_layout();
        match self.wrap_workspace_children(previous, layout) {
            Some(wrapper_key) => {
                self.selected_key = Some(wrapper_key);
                true
            }
            None => self.set_root_container_layout(layout),
        }
    }

    /// Split an explicit command target.
    pub(in crate::layout) fn split_target(
        &mut self,
        layout: Layout,
        target: TreeCommandTarget,
        root_policy: RootPolicy,
    ) -> bool {
        self.clear_focus_history();

        match target {
            TreeCommandTarget::Workspace => match root_policy {
                RootPolicy::ImplicitWorkspace => self.split_workspace_container(layout),
                RootPolicy::MaterialContainer => false,
            },
            TreeCommandTarget::Container(key) => {
                self.split_selected_container(key, layout, root_policy)
            }
            TreeCommandTarget::Leaf(key) => {
                debug_assert!(matches!(self.get_node(key), Some(NodeData::Leaf(_))));
                self.split_focused_with_policy(layout, root_policy)
            }
        }
    }

    /// Split the focused node using the root behavior of the owning space.
    pub(super) fn split_focused_with_policy(
        &mut self,
        layout: Layout,
        root_policy: RootPolicy,
    ) -> bool {
        self.clear_focus_history();

        if self.is_empty() {
            return match root_policy {
                RootPolicy::ImplicitWorkspace => self.set_root_container_layout(layout),
                RootPolicy::MaterialContainer => false,
            };
        }

        if root_policy == RootPolicy::MaterialContainer && self.focus_is_root() {
            return self.split_root_container(layout);
        }

        self.split_focused(layout)
    }

    /// Split a material root container, preserving selected command context on
    /// the original root node.
    pub(in crate::layout) fn split_root_container(&mut self, layout: Layout) -> bool {
        self.clear_focus_history();
        let root_key = self.root;

        let Some(container) = self.get_container_mut(root_key) else {
            return false;
        };
        container.set_layout_explicit(layout);
        self.selected_key = Some(root_key);
        if let Some(focused_key) = self.focused_key {
            self.sync_container_focus_from_key(focused_key);
        }
        true
    }

    /// Split the focused container in a direction
    pub(in crate::layout) fn split_focused(&mut self, layout: Layout) -> bool {
        self.clear_focus_history();
        if self.is_empty() {
            // Same rule as the workspace route: a split on an empty workspace only records
            // the orientation. The first window is a plain child of the workspace and gets
            // no wrapper; the orientation materializes when a second window arrives.
            self.set_root_container_layout(layout);
            return true;
        }

        let Some(focused_key) = self.effective_focused_key() else {
            return false;
        };

        let Some(parent_key) = self.parent_of(focused_key) else {
            return false;
        };
        let Some(parent) = self.get_container(parent_key) else {
            return false;
        };

        let parent_layout = parent.layout();
        let is_lone_child = parent.child_count() == 1;

        // i3's `tree_split`, and the workspace goes through it like any other parent. A
        // window alone in a split has nothing to be separated from, so the command only
        // (re)states that container's orientation; with siblings present, or a parent that
        // is tabbed or stacked, it builds a container. Measured, including when the parent
        // already has the requested orientation and regardless of how it got it — tiri used
        // to treat a parent whose layout had been set explicitly as already-split and do
        // nothing, which the differential fuzz caught on its first script.
        if matches!(parent_layout, Layout::SplitH | Layout::SplitV) && is_lone_child {
            if let Some(container) = self.get_container_mut(parent_key) {
                container.set_layout_explicit(layout);
            }
            return true;
        }

        // Otherwise put the leaf inside a new explicit split container in its own slot.
        let Some(child_idx) = self.child_index(parent_key, focused_key) else {
            return false;
        };
        let Some(wrapper_key) = self.wrap_child_in_container(parent_key, child_idx, layout) else {
            return false;
        };
        if let Some(wrapper) = self.get_container_mut(wrapper_key) {
            wrapper.mark_user_created();
        }

        self.focus_node_key(focused_key);
        true
    }

    /// Change the layout of the container holding the focused leaf.
    pub(in crate::layout) fn set_focused_layout_with_policy(
        &mut self,
        layout: Layout,
        root_policy: RootPolicy,
    ) -> bool {
        if self.is_empty() {
            return self.set_root_container_layout(layout);
        }

        let Some(focused_key) = self.effective_focused_key() else {
            return false;
        };

        self.set_layout_of_parent_of(focused_key, layout, root_policy)
    }

    /// Give `key`'s parent the layout `layout`.
    ///
    /// Measured against sway 1.11: this is what `layout X` does, whether `key` is a window
    /// or a container that `focus parent` selected. The workspace is the one parent that
    /// cannot be handed a layout this way — a container with the new layout takes the
    /// workspace's children instead, and the workspace keeps its own orientation. That
    /// holds for splits and tabbed/stacked alike, with one child or several.
    ///
    /// Restating the layout the workspace already has is the exception, and does nothing:
    /// there is no orientation to express that is not already expressed.
    fn set_layout_of_parent_of(
        &mut self,
        key: NodeKey,
        layout: Layout,
        root_policy: RootPolicy,
    ) -> bool {
        let parent_key = self.parent_of(key);

        if root_policy == RootPolicy::ImplicitWorkspace
            && parent_key.is_none_or(|parent| parent == self.root)
        {
            let root_layout = self.root_container_layout();
            if root_layout == layout {
                return false;
            }
            return self.wrap_workspace_children(layout, root_layout).is_some();
        }

        // The root is a leaf under a policy that wants a material container: wrap it so
        // there is a container to carry the layout.
        let Some(parent_key) = parent_key else {
            return self.set_root_container_layout(layout);
        };

        if let Some(container) = self.get_container_mut(parent_key) {
            container.set_layout_explicit(layout);
            return true;
        }

        false
    }

    /// Toggle between horizontal and vertical split for the focused container.
    #[cfg(test)]
    pub(in crate::layout) fn toggle_split_layout(&mut self) -> bool {
        let target = self.command_target(RootPolicy::MaterialContainer);
        self.toggle_split_for_target(target, RootPolicy::MaterialContainer)
    }

    pub(in crate::layout) fn toggle_split_for_target(
        &mut self,
        target: TreeCommandTarget,
        root_policy: RootPolicy,
    ) -> bool {
        let Some((target_key, current)) = self.toggle_source(target, root_policy) else {
            return false;
        };

        let next = match current {
            Layout::SplitH => Layout::SplitV,
            Layout::SplitV => Layout::SplitH,
            // Each container remembers the split it had. Reaching for the workspace's memory
            // when a container has none was reading another node's answer to another command.
            Layout::Tabbed | Layout::Stacked => target_key
                .and_then(|key| self.get_container(key))
                .and_then(|container| container.prev_split_layout())
                .unwrap_or(Layout::SplitH),
        };

        if matches!(current, Layout::Tabbed | Layout::Stacked) {
            if let Some(container) = target_key.and_then(|key| self.get_container_mut(key)) {
                container.set_layout_explicit(next);
                return true;
            }
            return false;
        }

        self.set_layout_for_target(next, target, root_policy)
    }

    pub(in crate::layout) fn toggle_layout_all_for_target(
        &mut self,
        target: TreeCommandTarget,
        root_policy: RootPolicy,
    ) -> bool {
        let Some((target_key, current)) = self.toggle_source(target, root_policy) else {
            return false;
        };

        let next = current.next_in_cycle();
        if let Some(container) = target_key.and_then(|key| self.get_container_mut(key)) {
            container.set_layout_explicit(next);
            return true;
        }

        self.set_layout_for_target(next, target, root_policy)
    }

    /// The container a layout toggle reads its current layout from, and that layout.
    ///
    /// A leaf sitting directly on the workspace has no container between it and the
    /// workspace, so the toggle starts from the workspace's own orientation — and lands
    /// through `set_layout_for_target`, which knows to build the container sway builds.
    fn toggle_source(
        &self,
        target: TreeCommandTarget,
        root_policy: RootPolicy,
    ) -> Option<(Option<NodeKey>, Layout)> {
        let key = self.layout_container_key_for_target(target, root_policy);
        match key.and_then(|key| self.get_container(key)) {
            Some(container) => Some((key, container.layout())),
            None if root_policy == RootPolicy::ImplicitWorkspace => {
                Some((None, self.root_container_layout()))
            }
            None => None,
        }
    }

    /// Layout of the container that currently owns the focused leaf (if any).
    pub(in crate::layout) fn focused_layout(&self) -> Option<Layout> {
        let focused_key = self.effective_focused_key()?;
        // A root leaf has no owning container, so the root's own layout is what applies.
        let container_key = self.parent_of(focused_key).unwrap_or(self.root);
        self.get_container(container_key).map(|c| c.layout())
    }

    /// Whether the focused container should accept new splits.
    pub(in crate::layout) fn focused_container_allows_splits(&self) -> bool {
        let Some(focused_key) = self.effective_focused_key() else {
            return false;
        };
        let Some(container_key) = self.parent_of(focused_key) else {
            return false;
        };

        let Some(container) = self.get_container(container_key) else {
            return false;
        };
        container.child_count() > 1 || container.is_user_container()
    }
}
