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

        if Some(container_key) == self.root {
            if root_policy == RootPolicy::MaterialContainer {
                return false;
            }

            let focus_key = self.focused_key.or_else(|| self.first_leaf_key());

            let mut outer = ContainerData::new(layout);
            outer.add_child(container_key);
            let outer_key = self.insert_node(NodeData::Container(outer));
            self.set_parent(container_key, Some(outer_key));
            self.set_parent(outer_key, None);
            self.root = Some(outer_key);

            if let Some(focus_key) = focus_key {
                self.focus_node_key(focus_key);
            }
            // Preserve command context on the originally selected top-level container.
            self.selected_key = Some(container_key);
            return true;
        }

        let Some(parent_key) = self.parent_of(container_key) else {
            return false;
        };
        let Some(parent) = self.get_container_mut(parent_key) else {
            return false;
        };
        parent.set_layout_explicit(layout);
        true
    }

    pub(in crate::layout) fn set_root_container_layout(&mut self, layout: Layout) -> bool {
        let Some(root_key) = self.root else {
            return false;
        };
        let Some(root) = self.get_container_mut(root_key) else {
            return false;
        };
        root.set_layout_explicit(layout);
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
                if Some(key) == self.root && root_policy == RootPolicy::MaterialContainer {
                    return None;
                }
                if Some(key) == self.root {
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
                self.parent_of(key).or(self.root)
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
        if container.child_count() != 1 || !container.preserve_on_single() {
            return None;
        }

        match container.layout() {
            Layout::SplitH | Layout::SplitV => Some(container.layout()),
            _ => None,
        }
    }

    pub(super) fn ensure_root_container_with_layout(&mut self, layout: Layout) -> bool {
        if let Some(root_key) = self.root {
            if matches!(self.get_node(root_key), Some(NodeData::Leaf(_))) {
                let old_root_key = self.take_root();
                let mut container = ContainerData::new(layout);
                container.mark_preserve_on_single();
                container.add_child(old_root_key);
                let container_key = self.insert_node(NodeData::Container(container));
                self.set_parent(old_root_key, Some(container_key));
                self.set_parent(container_key, None);
                self.root = Some(container_key);
                self.focus_node_key(old_root_key);
                return true;
            }
        }
        false
    }

    /// If selection points to the root container, wrap it into a SplitH parent so inserts
    /// happen as siblings of that container after `focus parent`.
    pub(super) fn ensure_selected_root_has_parent_for_sibling_insert(&mut self) -> bool {
        let Some(selected_key) = self.selected_key else {
            return false;
        };
        if Some(selected_key) != self.root {
            return false;
        }
        if !matches!(self.get_node(selected_key), Some(NodeData::Container(_))) {
            return false;
        }

        let old_root_key = selected_key;
        let layout = self.pending_layout.take().unwrap_or(Layout::SplitH);
        self.pending_layout_wrap_on_split = false;
        let mut container = ContainerData::new(layout);
        container.add_child(old_root_key);

        let container_key = self.insert_node(NodeData::Container(container));
        self.set_parent(old_root_key, Some(container_key));
        self.set_parent(container_key, None);
        self.root = Some(container_key);

        self.resync_focus();

        true
    }

    /// Wrap the root container into a SplitH parent so sibling insertions can
    /// target the root as a first-class container.
    pub(in crate::layout) fn wrap_root_for_sibling_insert(&mut self) -> bool {
        let Some(root_key) = self.root else {
            return false;
        };
        if !matches!(self.get_node(root_key), Some(NodeData::Container(_))) {
            return false;
        }
        self.selected_key = Some(root_key);
        self.ensure_selected_root_has_parent_for_sibling_insert()
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
    pub(in crate::layout) fn wrap_workspace_children(
        &mut self,
        wrapper_layout: Layout,
        root_layout: Layout,
    ) -> bool {
        let Some(root_key) = self.root else {
            return false;
        };

        // A lone window is the workspace's only child, so give it a container root first
        // and the wrap below has something to move.
        if matches!(self.get_node(root_key), Some(NodeData::Leaf(_)))
            && !self.ensure_root_container_with_layout(self.workspace_layout())
        {
            return false;
        }
        let Some(root_key) = self.root else {
            return false;
        };

        let (old_children, old_focus_stack, old_child_percents, root_geometry) = {
            let Some(root) = self.get_container_mut(root_key) else {
                return false;
            };
            if root.children.is_empty() {
                return false;
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
        wrapper.mark_preserve_on_single();
        wrapper.ensure_focus_stack();

        let wrapper_children = wrapper.children.clone();
        let wrapper_key = self.insert_node(NodeData::Container(wrapper));
        for child in wrapper_children {
            self.set_parent(child, Some(wrapper_key));
        }

        let Some(root) = self.get_container_mut(root_key) else {
            return false;
        };
        root.layout = root_layout;
        root.children.push(wrapper_key);
        root.focus_stack.push(wrapper_key);
        root.child_percents.push(1.0);
        root.ensure_focus_stack();
        self.set_parent(wrapper_key, Some(root_key));

        if let Some(focused_key) = self.focused_key {
            self.sync_container_focus_from_key(focused_key);
        }

        true
    }

    pub(in crate::layout) fn collapse_redundant_root_single_child_split(&mut self) -> bool {
        let mut changed = false;

        while let Some(root_key) = self.root {
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

            self.set_parent(child_key, None);
            self.root = Some(child_key);
            if self.selected_key == Some(root_key) {
                self.selected_key = Some(child_key);
            }
            self.nodes.remove(root_key);
            self.parents.remove(root_key);
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

        if Some(selected_key) == self.root {
            return match root_policy {
                RootPolicy::ImplicitWorkspace => {
                    self.pending_layout = Some(layout);
                    self.pending_layout_wrap_on_split = false;
                    true
                }
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
        wrapper.mark_preserve_on_single();
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

    pub(super) fn wrap_root_node_with_layout(
        &mut self,
        layout: Layout,
        preserve_selection_on_old_root: bool,
    ) -> bool {
        let Some(old_root_key) = self.root else {
            return false;
        };

        let focus_key = self.focused_key.or_else(|| self.first_leaf_key());

        let mut wrapper = ContainerData::new(layout);
        wrapper.mark_preserve_on_single();
        wrapper.add_child(old_root_key);

        let wrapper_key = self.insert_node(NodeData::Container(wrapper));
        self.set_parent(old_root_key, Some(wrapper_key));
        self.set_parent(wrapper_key, None);
        self.root = Some(wrapper_key);

        if let Some(focus_key) = focus_key {
            self.focus_node_key(focus_key);
        } else {
            self.focus_first_leaf();
        }

        if preserve_selection_on_old_root {
            self.selected_key = Some(old_root_key);
        }

        true
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
                RootPolicy::ImplicitWorkspace => {
                    self.pending_layout = Some(layout);
                    self.pending_layout_wrap_on_split = true;
                    true
                }
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

        if self.root.is_none() {
            return match root_policy {
                RootPolicy::ImplicitWorkspace => {
                    self.pending_layout = Some(layout);
                    self.pending_layout_wrap_on_split = true;
                    true
                }
                RootPolicy::MaterialContainer => false,
            };
        }

        if root_policy == RootPolicy::MaterialContainer && self.focus_is_root() {
            return self.wrap_root_node_with_layout(layout, false);
        }

        self.split_focused(layout)
    }

    /// Split a material root container, preserving selected command context on
    /// the original root node.
    pub(in crate::layout) fn split_root_container(&mut self, layout: Layout) -> bool {
        self.clear_focus_history();
        let Some(root_key) = self.root else {
            return false;
        };

        if let Some(container) = self.get_container_mut(root_key) {
            container.set_layout_explicit(layout);
            self.selected_key = Some(root_key);
            if let Some(focused_key) = self.focused_key {
                self.sync_container_focus_from_key(focused_key);
            }
            return true;
        }

        self.wrap_root_node_with_layout(layout, true)
    }

    /// Split the focused container in a direction
    pub(in crate::layout) fn split_focused(&mut self, layout: Layout) -> bool {
        self.clear_focus_history();
        if self.root.is_none() {
            // Same rule as the workspace route: a split on an empty workspace only records
            // the orientation. The first window is a plain child of the workspace and gets
            // no wrapper; the orientation materializes when a second window arrives.
            self.set_workspace_layout_hint(layout);
            return true;
        }

        let Some(focused_key) = self.effective_focused_key() else {
            return false;
        };

        // The window is alone on the workspace, so the workspace is the container being
        // split and it just changes orientation — measured against sway 1.11, which builds
        // no container here and keeps the orientation after the window closes.
        let Some(parent_key) = self.parent_of(focused_key) else {
            self.set_workspace_layout_hint(layout);
            return true;
        };
        let Some(parent) = self.get_container(parent_key) else {
            return false;
        };

        let parent_layout = parent.layout();
        let is_lone_child = parent.child_count() == 1;
        let restates_explicit_split =
            parent_layout == layout && parent.preserve_on_single() && Some(parent_key) != self.root;

        // The leaf either already sits alone in a split, or its container came from an
        // explicit split of this same orientation. Either way the command only (re)states
        // that container's orientation; wrapping again would add a redundant one-child
        // level around the leaf.
        if matches!(parent_layout, Layout::SplitH | Layout::SplitV)
            && (is_lone_child || restates_explicit_split)
        {
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
            wrapper.mark_preserve_on_single();
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
        if self.root.is_none() {
            self.pending_layout = Some(layout);
            self.pending_layout_wrap_on_split = false;
            return true;
        }

        let Some(focused_key) = self.effective_focused_key() else {
            return false;
        };

        // Measured against sway 1.11: a window whose parent is the workspace cannot hand
        // the workspace a layout — a container with the new layout takes the workspace's
        // children instead, and the workspace keeps its own orientation. This holds for
        // splits and for tabbed/stacked alike, with one child or several. A real container
        // parent, floating roots included, just takes the layout.
        //
        // Restating the layout the workspace already has is the one exception, and it does
        // nothing at all: there is no orientation to express that is not already expressed.
        let parent_key = self.parent_of(focused_key);
        if root_policy == RootPolicy::ImplicitWorkspace
            && parent_key.is_none_or(|key| Some(key) == self.root)
        {
            let root_layout = self.root_container_layout();
            if root_layout == layout {
                return false;
            }
            return self.wrap_workspace_children(layout, root_layout);
        }

        // The root is a leaf under a policy that wants a material container: wrap it so
        // there is a container to carry the layout.
        let Some(parent_key) = parent_key else {
            return self.ensure_root_container_with_layout(layout);
        };

        let target_key = parent_key;

        if let Some(container) = self.get_container_mut(target_key) {
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
        if self.root.is_none() {
            if target != TreeCommandTarget::Workspace {
                return false;
            }
            let next = match self.pending_layout.unwrap_or(Layout::SplitH) {
                Layout::SplitH => Layout::SplitV,
                _ => Layout::SplitH,
            };
            self.pending_layout = Some(next);
            self.pending_layout_wrap_on_split = false;
            return true;
        }

        let Some((target_key, current)) = self.toggle_source(target, root_policy) else {
            return false;
        };

        let next = match current {
            Layout::SplitH => Layout::SplitV,
            Layout::SplitV => Layout::SplitH,
            Layout::Tabbed | Layout::Stacked => target_key
                .and_then(|key| self.get_container(key))
                .and_then(|container| container.prev_split_layout())
                .unwrap_or_else(|| self.workspace_prev_split_layout()),
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
        if self.root.is_none() {
            if target != TreeCommandTarget::Workspace {
                return false;
            }
            let next = self
                .pending_layout
                .unwrap_or(Layout::SplitH)
                .next_in_cycle();
            self.pending_layout = Some(next);
            self.pending_layout_wrap_on_split = false;
            return true;
        }

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
        let container_key = self.parent_of(focused_key).unwrap_or(self.root?);
        self.get_container(container_key).map(|c| c.layout())
    }

    /// Whether the focused container should accept new splits.
    pub(in crate::layout) fn focused_container_allows_splits(&self) -> bool {
        let Some(focused_key) = self.effective_focused_key() else {
            return false;
        };
        // A root leaf has no owning container to split into.
        let container_key = match self.parent_of(focused_key) {
            Some(parent_key) => parent_key,
            None => match self.root {
                Some(root_key)
                    if matches!(self.get_node(root_key), Some(NodeData::Container(_))) =>
                {
                    root_key
                }
                _ => return false,
            },
        };

        let Some(container) = self.get_container(container_key) else {
            return false;
        };
        container.child_count() > 1 || container.preserve_on_single()
    }
}
