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
        self.flatten_layout_parent_once(target, root_policy);
        self.set_layout_for_prepared_target(layout, target, root_policy)
    }

    fn set_layout_for_prepared_target(
        &mut self,
        layout: Layout,
        target: TreeCommandTarget,
        root_policy: RootPolicy,
    ) -> bool {
        match target {
            // The workspace itself is selected, so it takes the layout. No container is
            // built: the workspace is the container, and it remembers the split it was on
            // the way out like any other.
            TreeCommandTarget::Workspace => self.set_root_container_layout(layout),
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

    /// Sway 1.12's one-off flatten at the start of `cmd_layout`.
    ///
    /// If the container whose layout the command would change is alone in another real
    /// container, and itself has only one child, remove the inner level. The command then
    /// lands on its parent. This is deliberately one operation, not tree normalization:
    /// deeper single-child chains remain, just as they do in i3 and sway.
    fn flatten_layout_parent_once(&mut self, target: TreeCommandTarget, root_policy: RootPolicy) {
        let Some(container_key) = self.layout_container_key_for_target(target, root_policy) else {
            return;
        };
        let Some(child_key) = self
            .get_container(container_key)
            .filter(|container| container.child_count() == 1)
            .and_then(ContainerData::focused_child_key)
        else {
            return;
        };
        let Some(parent_key) = self.parent_of(container_key) else {
            return;
        };

        // Tiri represents the workspace with a root ContainerData. Sway's workspace is a
        // different type and is not the `pending.parent` tested by cmd_layout, so it must
        // not satisfy the outer-container half of this rule.
        if root_policy == RootPolicy::ImplicitWorkspace && parent_key == self.root {
            return;
        }
        let Some(_) = self
            .get_container(parent_key)
            .filter(|parent| parent.child_count() == 1)
        else {
            return;
        };
        if !self.replace_child_node(parent_key, container_key, child_key) {
            return;
        }
        if self.selected_key == Some(container_key) {
            self.selected_key = Some(child_key);
        }
        if self.focused_key == Some(container_key) {
            self.focused_key = Some(child_key);
        }
        self.nodes.remove(container_key);
        self.parents.remove(container_key);
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

        let (old_children, old_focus_stack, old_fractions) = {
            let root = self.get_container_mut(root_key)?;
            if root.children.is_empty() {
                return None;
            }

            (
                std::mem::take(&mut root.children),
                std::mem::take(&mut root.focus_stack),
                std::mem::take(&mut root.fractions),
            )
        };

        let mut wrapper = ContainerData::new(wrapper_layout);
        wrapper.children = old_children;
        wrapper.focus_stack = old_focus_stack;
        wrapper.fractions = old_fractions;
        wrapper.set_layout(wrapper_layout);
        wrapper.resolve_child_percents();
        wrapper.ensure_focus_stack();
        // No box. `workspace_wrap_children` builds the wrapper with `container_create`, which
        // zeroes `pending`, and hands it nothing but the workspace's layout — the box arrives
        // when the workspace is next arranged. Copying the workspace's in advance was
        // invisible for as long as the arrange always followed; it stops being invisible the
        // moment there is a fullscreen and the arrange does not descend.

        let wrapper_children = wrapper.children.clone();
        let wrapper_key = self.insert_node(NodeData::Container(wrapper));
        for child in wrapper_children {
            self.set_parent(child, Some(wrapper_key));
        }

        let root = self.get_container_mut(root_key)?;
        root.set_layout(root_layout);
        root.insert_child(0, wrapper_key);
        root.ensure_focus_stack();
        self.set_parent(wrapper_key, Some(root_key));

        if let Some(focused_key) = self.focused_key {
            self.sync_container_focus_from_key(focused_key);
        }

        Some(wrapper_key)
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

        if self.child_index(parent_key, selected_key).is_none() {
            return false;
        }

        let mut wrapper = ContainerData::new(layout);
        wrapper.mark_user_created();
        if self
            .wrap_child_in_new_container(parent_key, selected_key, wrapper)
            .is_none()
        {
            return false;
        }

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
        let mut wrapper = ContainerData::new(layout);
        wrapper.mark_user_created();
        if self
            .wrap_child_in_new_container(parent_key, focused_key, wrapper)
            .is_none()
        {
            return false;
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
        self.flatten_layout_parent_once(target, root_policy);
        let Some((source_key, current)) = self.toggle_source(target, root_policy) else {
            return false;
        };

        let next = self.toggled_split_layout(current, source_key);
        self.apply_toggled_layout(next, source_key, target, root_policy)
    }

    pub(in crate::layout) fn toggle_layout_all_for_target(
        &mut self,
        target: TreeCommandTarget,
        root_policy: RootPolicy,
    ) -> bool {
        self.flatten_layout_parent_once(target, root_policy);
        let Some((source_key, current)) = self.toggle_source(target, root_policy) else {
            return false;
        };

        self.apply_toggled_layout(current.next_in_cycle(), source_key, target, root_policy)
    }

    /// Write back the layout a toggle arrived at.
    ///
    /// The container it read from is the container the command is about, so an ordinary one
    /// just changes type. The workspace is the exception, and not because it is special: a
    /// toggle that reads from it has either the workspace itself selected, which changes its
    /// orientation, or a window sitting on it, which gets the container sway builds instead.
    /// Telling those two apart is `set_layout_for_target`'s job, so they go there.
    fn apply_toggled_layout(
        &mut self,
        layout: Layout,
        source_key: Option<NodeKey>,
        target: TreeCommandTarget,
        root_policy: RootPolicy,
    ) -> bool {
        match source_key {
            Some(key) if key != self.root => self
                .get_container_mut(key)
                .map(|container| container.set_layout_explicit(layout))
                .is_some(),
            _ => self.set_layout_for_prepared_target(layout, target, root_policy),
        }
    }

    /// sway's `toggle_split_layout`, the rule behind `layout toggle split`.
    ///
    /// A split becomes the other split. Tabs and stacks go back to the split the container
    /// had before — every container remembers its own, set whenever it stops being a split —
    /// and a container that has never been one asks the screen, which is sway's last resort
    /// once the configured default orientation is out of the picture.
    pub(in crate::layout) fn toggled_split_layout(
        &self,
        current: Layout,
        container_key: Option<NodeKey>,
    ) -> Layout {
        match current {
            Layout::SplitH => Layout::SplitV,
            Layout::SplitV => Layout::SplitH,
            Layout::Tabbed | Layout::Stacked => {
                let along_the_screen = if self.view_size.h > self.view_size.w {
                    Layout::SplitV
                } else {
                    Layout::SplitH
                };
                container_key
                    .and_then(|key| self.get_container(key))
                    .and_then(ContainerData::prev_split_layout)
                    .unwrap_or(along_the_screen)
            }
        }
    }

    /// The container a layout toggle reads its current layout from, and that layout.
    ///
    /// A leaf sitting directly on the workspace has no container between it and the
    /// workspace, so the toggle reads the workspace's own orientation — and its memory of
    /// the split it used to be, which is the same field on the same node as anywhere else.
    fn toggle_source(
        &self,
        target: TreeCommandTarget,
        root_policy: RootPolicy,
    ) -> Option<(Option<NodeKey>, Layout)> {
        let key = self.layout_container_key_for_target(target, root_policy);
        match key.and_then(|key| self.get_container(key)) {
            Some(container) => Some((key, container.layout())),
            None if root_policy == RootPolicy::ImplicitWorkspace => {
                Some((Some(self.root), self.root_container_layout()))
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
