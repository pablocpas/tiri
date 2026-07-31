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
    /// Apply a layout command to the selected container command target.
    ///
    /// A selected top-level tiling container resolves through the implicit
    /// workspace parent, so the root is wrapped and the previous selection is
    /// preserved as command context.
    pub(in crate::layout) fn set_layout_for_selected_container(&mut self, layout: Layout) -> bool {
        let Some(selected_key) = self.selected_container_key() else {
            return false;
        };
        self.set_layout_for_container_target(selected_key, layout, RootPolicy::ImplicitWorkspace)
    }

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
                self.set_focused_layout(layout)
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
                let path = self.find_node_path(key)?;
                if path.is_empty() {
                    self.root
                } else {
                    self.node_key_for_path_or_root(&path[..path.len() - 1])
                }
            }
        }
    }

    pub(in crate::layout) fn parent_layout_for_path(&self, path: &[usize]) -> Option<Layout> {
        if path.is_empty() {
            return None;
        }

        let parent_path = &path[..path.len() - 1];
        let parent_key = if parent_path.is_empty() {
            self.root?
        } else {
            self.get_node_key_at_path(parent_path)?
        };
        self.get_container(parent_key).map(|c| c.layout())
    }

    pub(in crate::layout) fn single_child_split_layout_for_path(
        &self,
        path: &[usize],
    ) -> Option<Layout> {
        if path.is_empty() {
            return None;
        }

        let parent_path = &path[..path.len() - 1];
        let parent_key = if parent_path.is_empty() {
            self.root?
        } else {
            self.get_node_key_at_path(parent_path)?
        };

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

        if let Some(key) = self.focused_key {
            self.sync_container_focus_from_key(key);
        } else {
            self.focus_first_leaf();
        }

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

    /// Apply a workspace-level layout change by keeping the synthetic root as
    /// the workspace node and moving its current children under an explicit
    /// wrapper with the old layout.
    pub(in crate::layout) fn wrap_synthetic_root_children_for_workspace_layout(
        &mut self,
        layout: Layout,
    ) -> bool {
        let Some(root_key) = self.root else {
            return false;
        };
        if !self.is_synthetic_root_container_key(root_key) {
            return false;
        }

        let (old_layout, old_children, old_focus_stack, old_child_percents, root_geometry) = {
            let Some(root) = self.get_container_mut(root_key) else {
                return false;
            };
            if root.children.is_empty() {
                return false;
            }

            (
                root.layout,
                std::mem::take(&mut root.children),
                std::mem::take(&mut root.focus_stack),
                std::mem::take(&mut root.child_percents),
                root.geometry,
            )
        };

        let mut wrapper = ContainerData::new(old_layout);
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
        root.layout = layout;
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

    /// Collapse a one-child command target whose parent is also a one-child
    /// container, then return the parent that now owns the promoted child.
    pub(super) fn collapse_single_child_command_target_once(
        &mut self,
        container_key: NodeKey,
    ) -> Option<NodeKey> {
        let child_key = self.get_container(container_key).and_then(|container| {
            (container.child_count() == 1)
                .then(|| container.children().first().copied())
                .flatten()
        })?;

        let parent_key = self.parent_of(container_key)?;
        let parent_child_count = self
            .get_container(parent_key)
            .map(|container| container.child_count())?;
        if parent_child_count != 1 {
            return None;
        }

        let parent_idx = self.child_index(parent_key, container_key)?;
        if let Some(parent) = self.get_container_mut(parent_key) {
            parent.children[parent_idx] = child_key;
            if let Some(pos) = parent
                .focus_stack
                .iter()
                .position(|key| *key == container_key)
            {
                parent.focus_stack[pos] = child_key;
            } else if !parent.focus_stack.contains(&child_key) {
                parent.focus_stack.push(child_key);
            }
            parent.ensure_focus_stack();
        }

        self.set_parent(child_key, Some(parent_key));
        self.nodes.remove(container_key);
        self.parents.remove(container_key);

        if self.selected_key == Some(container_key) {
            self.selected_key = Some(child_key);
        }
        if self.focused_key == Some(container_key) {
            self.focused_key = self.leaf_under_key(child_key).or(Some(child_key));
        }

        Some(parent_key)
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

        if root_policy == RootPolicy::MaterialContainer && self.focus_path().is_empty() {
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
            self.pending_layout = Some(layout);
            self.pending_layout_wrap_on_split = true;
            return true;
        }

        let focus_path = self.focus_path();

        // Special case: if root is a leaf, wrap it in a container immediately.
        if focus_path.is_empty() {
            return self.ensure_root_container_with_layout(layout);
        }

        let parent_path = &focus_path[..focus_path.len() - 1];
        let child_idx = *focus_path.last().unwrap();

        let Some(parent_key) = self.node_key_for_path_or_root(parent_path) else {
            return false;
        };

        let (parent_layout, parent_child_count, parent_preserve_on_single) =
            match self.get_container(parent_key) {
                Some(container) => (
                    container.layout(),
                    container.child_count(),
                    container.preserve_on_single(),
                ),
                None => return false,
            };

        if matches!(parent_layout, Layout::SplitH | Layout::SplitV)
            && parent_layout == layout
            && (parent_child_count == 1
                || (parent_preserve_on_single && Some(parent_key) != self.root))
        {
            if let Some(container) = self.get_container_mut(parent_key) {
                // Repeating the same split direction should only refresh explicit intent, not
                // introduce an extra one-child wrapper around the focused leaf.
                container.set_layout_explicit(layout);
            }
            return true;
        }

        // Get the focused child key
        let focused_child_key = if let Some(container) = self.get_container(parent_key) {
            match container.child_key(child_idx) {
                Some(key) => key,
                None => return false,
            }
        } else {
            return false;
        };

        // Only split if it's a leaf
        if matches!(self.get_node(focused_child_key), Some(NodeData::Leaf(_))) {
            let parent_child_count = self
                .get_container(parent_key)
                .map(|container| container.child_count())
                .unwrap_or(0);

            if parent_child_count == 1 && matches!(parent_layout, Layout::SplitH | Layout::SplitV) {
                if let Some(container) = self.get_container_mut(parent_key) {
                    // Explicit split command on a single-child split container
                    // keeps that container around for future sibling inserts,
                    // even if the requested split orientation matches the current one.
                    container.set_layout_explicit(layout);
                }
                return true;
            }

            // Remove child from parent
            if let Some(container) = self.get_container_mut(parent_key) {
                container.remove_child(child_idx);
            }
            self.set_parent(focused_child_key, None);

            // Create new container with the leaf
            let mut new_container = ContainerData::new(layout);
            new_container.mark_preserve_on_single();
            new_container.add_child(focused_child_key);
            let new_container_key = self.insert_node(NodeData::Container(new_container));
            self.set_parent(focused_child_key, Some(new_container_key));

            // Insert new container back at same position
            if let Some(container) = self.get_container_mut(parent_key) {
                container.insert_child(child_idx, new_container_key);
            }
            self.set_parent(new_container_key, Some(parent_key));

            self.focus_node_key(focused_child_key);
            return true;
        }

        false
    }

    /// Change layout of focused container
    pub(in crate::layout) fn set_focused_layout(&mut self, layout: Layout) -> bool {
        if self.root.is_none() {
            self.pending_layout = Some(layout);
            self.pending_layout_wrap_on_split = false;
            return true;
        }

        let focus_path = self.focus_path();

        if focus_path.is_empty() {
            // Root is a leaf: always wrap immediately with the requested layout.
            let root_is_leaf = self
                .root
                .is_some_and(|root_key| matches!(self.get_node(root_key), Some(NodeData::Leaf(_))));
            if root_is_leaf {
                return self.ensure_root_container_with_layout(layout);
            }
        }

        // If focus is on a leaf, use parent container
        if let Some(node_key) = self.get_node_key_at_path(&focus_path) {
            if matches!(self.get_node(node_key), Some(NodeData::Leaf(_))) {
                // Get parent container
                if focus_path.is_empty() {
                    return false;
                }

                let parent_path = &focus_path[..focus_path.len() - 1];
                let Some(parent_key) = self.node_key_for_path_or_root(parent_path) else {
                    return false;
                };

                let target_key = if matches!(layout, Layout::Tabbed | Layout::Stacked) {
                    self.collapse_single_child_command_target_once(parent_key)
                        .unwrap_or(parent_key)
                } else {
                    parent_key
                };
                let target_is_root = Some(target_key) == self.root;

                let (parent_layout, parent_child_count, parent_preserve_on_single) = self
                    .get_container(target_key)
                    .map(|container| {
                        (
                            container.layout(),
                            container.child_count(),
                            container.preserve_on_single(),
                        )
                    })
                    .unwrap_or((layout, 0, false));

                if parent_layout == layout
                    && parent_child_count == 1
                    && parent_preserve_on_single
                    && target_is_root
                    && matches!(layout, Layout::SplitH | Layout::SplitV)
                {
                    if layout == Layout::SplitV {
                        let Some(child_idx) = self.child_index(target_key, node_key) else {
                            return false;
                        };

                        if let Some(container) = self.get_container_mut(target_key) {
                            container.remove_child(child_idx);
                        }
                        self.set_parent(node_key, None);

                        let mut nested = ContainerData::new(layout);
                        nested.mark_preserve_on_single();
                        nested.add_child(node_key);
                        let nested_key = self.insert_node(NodeData::Container(nested));
                        self.set_parent(node_key, Some(nested_key));

                        if let Some(container) = self.get_container_mut(target_key) {
                            container.insert_child(child_idx, nested_key);
                        }
                        self.set_parent(nested_key, Some(target_key));

                        self.focus_node_key(node_key);
                        return true;
                    }

                    if let Some(container) = self.get_container_mut(target_key) {
                        // Regression path: layout_splith on this
                        // explicit single-child root keeps the shape flat (no extra nesting).
                        container.set_layout_explicit(layout);
                    }
                    return true;
                }

                if let Some(container) = self.get_container_mut(target_key) {
                    container.set_layout_explicit(layout);
                    return true;
                }
            } else {
                // It's already a container, change its layout
                if let Some(container) = self.get_container_mut(node_key) {
                    container.set_layout_explicit(layout);
                    return true;
                }
            }
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

        let Some(target_key) = self.layout_container_key_for_target(target, root_policy) else {
            return false;
        };

        let current = match self.get_container(target_key) {
            Some(container) => container.layout(),
            None => return false,
        };

        let next = match current {
            Layout::SplitH => Layout::SplitV,
            Layout::SplitV => Layout::SplitH,
            Layout::Tabbed | Layout::Stacked => self
                .get_container(target_key)
                .and_then(|container| container.prev_split_layout())
                .unwrap_or(Layout::SplitH),
        };

        if matches!(current, Layout::Tabbed | Layout::Stacked) {
            if let Some(container) = self.get_container_mut(target_key) {
                container.set_layout_explicit(next);
                return true;
            }
            return false;
        }

        self.set_layout_for_target(next, target, root_policy)
    }

    /// Cycle focused container layout in sway-style order:
    /// SplitH -> SplitV -> Stacked -> Tabbed -> SplitH.
    #[cfg(test)]
    pub(in crate::layout) fn toggle_layout_all(&mut self) -> bool {
        let target = self.command_target(RootPolicy::MaterialContainer);
        self.toggle_layout_all_for_target(target, RootPolicy::MaterialContainer)
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
            let next = match self.pending_layout.unwrap_or(Layout::SplitH) {
                Layout::SplitH => Layout::SplitV,
                Layout::SplitV => Layout::Stacked,
                Layout::Stacked => Layout::Tabbed,
                Layout::Tabbed => Layout::SplitH,
            };
            self.pending_layout = Some(next);
            self.pending_layout_wrap_on_split = false;
            return true;
        }

        let Some(target_key) = self.layout_container_key_for_target(target, root_policy) else {
            return false;
        };

        let current = match self.get_container(target_key) {
            Some(container) => container.layout(),
            None => return false,
        };

        let next = match current {
            Layout::SplitH => Layout::SplitV,
            Layout::SplitV => Layout::Stacked,
            Layout::Stacked => Layout::Tabbed,
            Layout::Tabbed => Layout::SplitH,
        };

        if let Some(container) = self.get_container_mut(target_key) {
            container.set_layout_explicit(next);
            true
        } else {
            false
        }
    }

    /// Layout of the container that currently owns the focused leaf (if any).
    pub(in crate::layout) fn focused_layout(&self) -> Option<Layout> {
        let focus_path = self.focus_path();
        if focus_path.is_empty() {
            let root_key = self.root?;
            self.get_container(root_key).map(|c| c.layout())
        } else {
            let parent_path = &focus_path[..focus_path.len() - 1];
            let parent_key = if parent_path.is_empty() {
                self.root?
            } else {
                self.get_node_key_at_path(parent_path)?
            };
            self.get_container(parent_key).map(|c| c.layout())
        }
    }

    /// Whether the focused container should accept new splits.
    pub(in crate::layout) fn focused_container_allows_splits(&self) -> bool {
        let focus_path = self.focus_path();
        let container_key = if focus_path.is_empty() {
            let root_key = match self.root {
                Some(key) => key,
                None => return false,
            };
            if matches!(self.get_node(root_key), Some(NodeData::Container(_))) {
                root_key
            } else {
                return false;
            }
        } else {
            let parent_path = &focus_path[..focus_path.len() - 1];
            if parent_path.is_empty() {
                match self.root {
                    Some(key) => key,
                    None => return false,
                }
            } else {
                match self.get_node_key_at_path(parent_path) {
                    Some(key) => key,
                    None => return false,
                }
            }
        };

        let Some(container) = self.get_container(container_key) else {
            return false;
        };
        container.child_count() > 1 || container.preserve_on_single()
    }
}
