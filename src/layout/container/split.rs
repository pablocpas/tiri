//! Split, layout-toggle and root-wrapping commands.

use super::ContainerData;
use super::ContainerTree;
use super::FloatingGeometry;
use super::Layout;
use super::LayoutElement;
use super::NodeData;
use super::NodeKey;
use crate::layout::LayoutCycleEntry;

impl<W: LayoutElement> ContainerTree<W> {
    /// Sway's `split none`: flatten the target's single-child parent, continuing through
    /// single-child container ancestors but never through the workspace.
    ///
    /// The returned key is the branch root after the operation. Tiri needs that answer because
    /// a floating group keeps its stacking metadata outside the arena, while sway can replace
    /// the top-level floating container directly in the workspace list.
    pub(in crate::layout) fn unsplit_target(&mut self, target_key: NodeKey) -> Option<NodeKey> {
        if target_key == self.root || self.get_node(target_key).is_none() {
            return None;
        }

        let mut container_key = self.parent_of(target_key)?;
        if self
            .get_container(container_key)
            .is_none_or(|container| container.child_count() != 1)
        {
            return None;
        }

        self.clear_focus_history();
        let branch_root = loop {
            let child_key = self
                .get_container(container_key)
                .filter(|container| container.child_count() == 1)
                .and_then(|container| container.child_key(0))?;

            let parent_key = self.parent_of(container_key).filter(|_| {
                self.branch_root(container_key) != container_key
                    || !self.branch_is_addressable(container_key)
            });
            let Some(parent_key) = parent_key else {
                // A top-level floating view is a container in sway. Tiri represents a view as
                // a leaf and therefore retains an invisible group root when that is the child.
                // Clearing the explicit bit makes the wrapper disappear from command and IPC
                // semantics while preserving the leaf's NodeKey.
                if matches!(self.get_node(child_key), Some(NodeData::Leaf(_))) {
                    self.get_real_container_mut(container_key)?
                        .clear_user_created();
                    break container_key;
                }

                // A real child container can become the floating root without changing its
                // identity. Transfer only the root-owned geometry; all subtree state remains
                // on the child NodeKey.
                let old_floating_geometry = self
                    .get_real_container_mut(container_key)?
                    .floating_geometry
                    .take()?;
                let child_area = self.get_container(child_key)?.geometry;
                let floating_geometry =
                    FloatingGeometry::new(old_floating_geometry.working_area, child_area);
                self.get_real_container_mut(child_key)?.floating_geometry = Some(floating_geometry);
                self.set_parent(child_key, Some(self.root));
                self.transfer_fullscreen_to_replacement(container_key, child_key);
                if self.selected_key() == Some(container_key) {
                    self.seat.keep_selected(child_key);
                }
                if self.focused_key() == Some(container_key) {
                    self.seat
                        .redirect_focused_leaf(self.leaf_under_key(child_key));
                }
                self.remove_node_from_store(container_key);
                break child_key;
            };

            if !self.replace_child_node(parent_key, container_key, child_key) {
                return None;
            }
            if matches!(self.get_node(child_key), Some(NodeData::Leaf(_))) {
                self.preserve_leaf_node_geometry(child_key);
            }
            if self.selected_key() == Some(container_key) {
                self.seat.keep_selected(child_key);
            }
            if self.focused_key() == Some(container_key) {
                self.seat
                    .redirect_focused_leaf(self.leaf_under_key(child_key));
            }
            self.remove_node_from_store(container_key);

            // The workspace is the structural boundary. sway's top-level container has no
            // container parent, so `container_flatten` stops here rather than flattening it.
            if parent_key == self.root {
                break self.root;
            }
            if self
                .get_container(parent_key)
                .is_none_or(|container| container.child_count() != 1)
            {
                break self.branch_root(child_key);
            }
            container_key = parent_key;
        };

        self.readdress_leaf_layouts();
        self.prune_focus_order();
        Some(branch_root)
    }

    pub(super) fn set_layout_for_container_target(
        &mut self,
        container_key: NodeKey,
        layout: Layout,
    ) -> bool {
        if self.get_real_container(container_key).is_none() {
            return false;
        }

        if self.branch_root(container_key) == container_key
            && self.branch_is_addressable(container_key)
        {
            return false;
        }

        self.set_layout_of_parent_of(container_key, layout)
    }

    /// Hand a branch's root container a layout.
    ///
    /// The workspace carries its own orientation and so does a floating group's root; neither
    /// gets the user-created bit for it, because being addressable is not what that bit means.
    pub(in crate::layout) fn set_branch_root_layout(
        &mut self,
        branch_root: NodeKey,
        layout: Layout,
    ) -> bool {
        let Some(root) = self.get_container_mut(branch_root) else {
            return false;
        };
        root.set_layout(layout);
        true
    }

    pub(in crate::layout) fn set_root_container_layout(&mut self, layout: Layout) -> bool {
        let root_key = self.root;
        self.set_branch_root_layout(root_key, layout)
    }

    fn set_branch_root_layout_from_command(
        &mut self,
        branch_root: NodeKey,
        layout: Layout,
    ) -> bool {
        let Some(root) = self.get_container_mut(branch_root) else {
            return false;
        };
        root.set_layout_from_command(layout);
        true
    }

    pub(in crate::layout) fn set_root_layout_from_command(&mut self, layout: Layout) -> bool {
        self.set_branch_root_layout_from_command(self.root, layout)
    }

    pub(in crate::layout) fn set_layout_for_target(
        &mut self,
        layout: Layout,
        target: NodeKey,
    ) -> bool {
        self.flatten_layout_parent_once(target);
        self.set_layout_for_prepared_target(layout, target)
    }

    fn set_layout_for_prepared_target(&mut self, layout: Layout, target: NodeKey) -> bool {
        if target == self.root {
            // The workspace itself is selected, so it takes the layout. No container is
            // built: the workspace is the container, and it remembers the split it was on
            // the way out like any other.
            return self.set_root_layout_from_command(layout);
        }
        match self.get_node(target) {
            Some(NodeData::Workspace(_)) => false,
            Some(NodeData::Container(_)) => self.set_layout_for_container_target(target, layout),
            Some(NodeData::Leaf(_)) => self.set_layout_of_parent_of(target, layout),
            None => false,
        }
    }

    /// Sway 1.12's one-off flatten at the start of `cmd_layout`.
    ///
    /// If the container whose layout the command would change is alone in another real
    /// container, and itself has only one child, remove the inner level. The command then
    /// lands on its parent. This is deliberately one operation, not tree normalization:
    /// deeper single-child chains remain, just as they do in i3 and sway.
    fn flatten_layout_parent_once(&mut self, target: NodeKey) {
        let Some(container_key) = self.layout_container_key_for_target(target) else {
            return;
        };
        let Some(child_key) = self
            .get_container(container_key)
            .filter(|container| container.child_count() == 1)
            .and_then(|_| self.active_child(container_key))
        else {
            return;
        };
        let Some(parent_key) = self.parent_of(container_key) else {
            return;
        };

        // The workspace is not the `pending.parent` container tested by cmd_layout.
        if matches!(self.get_node(parent_key), Some(NodeData::Workspace(_))) {
            return;
        }
        let Some(_) = self
            .get_container(parent_key)
            .filter(|parent| parent.child_count() == 1)
        else {
            return;
        };
        // `container_replace` leaves the replacement's pending box alone. A leaf directly
        // under tabbed/stacked also has a decorated content rectangle; after reparenting,
        // that rectangle becomes the pending node box without an arrange in between.
        //
        // sway/tree/container.c:1534-1554
        if !self.replace_child_node(parent_key, container_key, child_key) {
            return;
        }
        if matches!(self.get_node(child_key), Some(NodeData::Leaf(_))) {
            self.preserve_leaf_node_geometry(child_key);
        }
        if self.selected_key() == Some(container_key) {
            self.seat.keep_selected(child_key);
        }
        if self.focused_key() == Some(container_key) {
            self.seat.redirect_focused_leaf(Some(child_key));
        }
        self.remove_node_from_store(container_key);
        self.readdress_leaf_layouts();
    }

    pub(super) fn layout_container_key_for_target(&self, target: NodeKey) -> Option<NodeKey> {
        if target == self.root {
            return None;
        }
        if self.branch_root(target) == target && self.branch_is_addressable(target) {
            return None;
        }
        match self.get_node(target) {
            Some(NodeData::Workspace(_)) => None,
            Some(NodeData::Container(_)) => self.parent_of(target),
            Some(NodeData::Leaf(_)) => {
                // A branch-root leaf has no owning container, so it is its own target.
                self.parent_of(target).or(Some(target))
            }
            None => None,
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

        let container = self.get_real_container(parent_key)?;
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
    pub(in crate::layout) fn wrap_branch_children(
        &mut self,
        branch_root: NodeKey,
        wrapper_layout: Layout,
        root_layout: Layout,
    ) -> Option<NodeKey> {
        let root_key = branch_root;

        let old_children = {
            let root = self.get_container_mut(root_key)?;
            if root.children.is_empty() {
                return None;
            }
            std::mem::take(&mut root.children)
        };

        let mut wrapper = ContainerData::new(wrapper_layout);
        wrapper.children = old_children;
        wrapper.set_layout(wrapper_layout);
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
        self.set_parent(wrapper_key, Some(root_key));

        Some(wrapper_key)
    }

    pub(in crate::layout) fn wrap_workspace_children(
        &mut self,
        wrapper_layout: Layout,
        root_layout: Layout,
    ) -> Option<NodeKey> {
        let root_key = self.root;
        self.wrap_branch_children(root_key, wrapper_layout, root_layout)
    }

    pub(super) fn split_selected_container(
        &mut self,
        selected_key: NodeKey,
        layout: Layout,
    ) -> bool {
        if !matches!(self.get_node(selected_key), Some(NodeData::Container(_))) {
            return false;
        }

        if self.branch_root(selected_key) == selected_key
            && self.branch_is_addressable(selected_key)
        {
            return self.split_branch_root(selected_key, layout);
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
        self.seat.keep_selected(selected_key);
        if self.focused_key().is_none() {
            if let Some(leaf_key) = self.leaf_under_key(selected_key) {
                self.seat.redirect_focused_leaf(Some(leaf_key));
            }
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
        if self
            .get_container(self.root)
            .is_some_and(|root| root.child_count() == 0)
        {
            self.get_container_mut(self.root)
                .expect("workspace root")
                .set_layout_from_empty_workspace_split(layout);
            return true;
        }

        let previous = self.root_container_layout();
        match self.wrap_workspace_children(previous, layout) {
            Some(wrapper_key) => {
                self.select_container(wrapper_key);
                true
            }
            None => self.set_root_container_layout(layout),
        }
    }

    /// Split an explicit command target.
    pub(in crate::layout) fn split_target(&mut self, layout: Layout, target: NodeKey) -> bool {
        self.clear_focus_history();

        let changed = if target == self.root {
            self.split_workspace_container(layout)
        } else {
            match self.get_node(target) {
                Some(NodeData::Workspace(_)) => false,
                Some(NodeData::Container(_)) => self.split_selected_container(target, layout),
                Some(NodeData::Leaf(_)) => self.split_leaf(target, layout),
                None => false,
            }
        };
        if changed {
            self.readdress_leaf_layouts();
        }
        changed
    }

    /// Split a branch's root container, keeping the command context on it.
    pub(in crate::layout) fn split_branch_root(
        &mut self,
        branch_root: NodeKey,
        layout: Layout,
    ) -> bool {
        self.clear_focus_history();

        let Some(container) = self.get_real_container_mut(branch_root) else {
            return false;
        };
        container.set_layout_explicit(layout);
        self.seat.keep_selected(branch_root);
        true
    }

    /// Split the focused container in a direction
    pub(in crate::layout) fn split_focused(&mut self, layout: Layout) -> bool {
        self.clear_focus_history();
        if self.is_empty() {
            // Same rule as the workspace route: a split on an empty workspace only records
            // the orientation. The first window is a plain child of the workspace and gets
            // no wrapper; the orientation materializes when a second window arrives.
            self.get_container_mut(self.root)
                .expect("workspace root")
                .set_layout_from_empty_workspace_split(layout);
            return true;
        }

        let Some(focused_key) = self.effective_focused_key() else {
            return false;
        };
        self.split_leaf(focused_key, layout)
    }

    /// i3's `tree_split` on one node.
    ///
    /// A window alone in a split has nothing to be separated from, so the command only
    /// (re)states that container's orientation; with siblings present, or a parent that is
    /// tabbed or stacked, it builds a container. Measured, including when the parent already
    /// has the requested orientation and regardless of how it got it — tiri used to treat a
    /// parent whose layout had been set explicitly as already-split and do nothing, which the
    /// differential fuzz caught on its first script.
    pub(super) fn split_leaf(&mut self, key: NodeKey, layout: Layout) -> bool {
        let Some(parent_key) = self.parent_of(key) else {
            // A branch whose root is the node itself: there is nothing above it to split
            // against, so the node's own orientation is all the command can state.
            return self.split_branch_root(key, layout);
        };
        let Some(parent) = self.get_container(parent_key) else {
            return false;
        };

        let parent_layout = parent.layout();
        let is_lone_child = parent.child_count() == 1;

        if matches!(parent_layout, Layout::SplitH | Layout::SplitV) && is_lone_child {
            if parent_key == self.root {
                if let Some(workspace) = self.get_container_mut(parent_key) {
                    workspace.set_layout(layout);
                }
            } else if let Some(container) = self.get_real_container_mut(parent_key) {
                container.set_layout_explicit(layout);
            }
            return true;
        }

        // Otherwise put the leaf inside a new explicit split container in its own slot.
        let mut wrapper = ContainerData::new(layout);
        wrapper.mark_user_created();
        if self
            .wrap_child_in_new_container(parent_key, key, wrapper)
            .is_none()
        {
            return false;
        }

        true
    }

    /// Change the layout of the container holding the focused leaf.
    #[cfg(test)]
    pub(in crate::layout) fn set_focused_layout(&mut self, layout: Layout) -> bool {
        if self.is_empty() {
            return self.set_root_container_layout(layout);
        }

        let Some(focused_key) = self.effective_focused_key() else {
            return false;
        };

        self.set_layout_of_parent_of(focused_key, layout)
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
    fn set_layout_of_parent_of(&mut self, key: NodeKey, layout: Layout) -> bool {
        let parent_key = self.parent_of(key);
        let lands_on_workspace = parent_key == Some(self.root);

        if lands_on_workspace && !self.branch_is_addressable(key) {
            let root_layout = self.root_container_layout();
            if root_layout == layout {
                return false;
            }
            return self.wrap_workspace_children(layout, root_layout).is_some();
        }

        // The branch root is a leaf: it has no container to carry the layout, so its own
        // layout is what the command can state.
        let Some(parent_key) = parent_key else {
            return self.set_branch_root_layout(key, layout);
        };

        if let Some(container) = self.get_real_container_mut(parent_key) {
            let changed = container.layout() != layout;
            container.set_layout_explicit_from_command(layout);

            // `cmd_layout` performs its one-level flatten before comparing layouts, but
            // arranges only inside `new_layout != old_layout`. A flatten followed by
            // restating the surviving parent's layout therefore changes the tree without
            // replacing the boxes its nodes were already holding.
            //
            // sway/commands/layout.c:134-196
            return changed;
        }

        false
    }

    /// Toggle between horizontal and vertical split for the focused container.
    #[cfg(test)]
    pub(in crate::layout) fn toggle_split_layout(&mut self) -> bool {
        let root = self.root;
        let target = self.command_target_in(root);
        self.toggle_split_for_target(target)
    }

    pub(in crate::layout) fn toggle_split_for_target(&mut self, target: NodeKey) -> bool {
        self.flatten_layout_parent_once(target);
        let Some((source_key, current)) = self.toggle_source(target) else {
            return false;
        };

        let next = self.toggled_split_layout(current, source_key);
        self.apply_toggled_layout(next, source_key, target)
    }

    pub(in crate::layout) fn toggle_layout_all_for_target(&mut self, target: NodeKey) -> bool {
        self.flatten_layout_parent_once(target);
        let Some((source_key, current)) = self.toggle_source(target) else {
            return false;
        };

        self.apply_toggled_layout(current.next_in_cycle(), source_key, target)
    }

    /// Restore the split remembered by the container targeted by `layout default`.
    pub(in crate::layout) fn set_default_layout_for_target(&mut self, target: NodeKey) -> bool {
        self.flatten_layout_parent_once(target);
        let Some((source_key, _)) = self.toggle_source(target) else {
            return false;
        };
        let Some(layout) = source_key
            .and_then(|key| self.get_container(key))
            .and_then(|container| container.prev_split_layout())
        else {
            return false;
        };

        self.apply_toggled_layout(layout, source_key, target)
    }

    /// Sway's explicit `layout toggle <cycle...>`.
    ///
    /// The current layout selects the entry after its first match. `split` matches either
    /// split orientation and, when selected as the destination, uses the same remembered-split
    /// rule as bare `layout toggle`. Invalid strings have already been discarded by the command
    /// parser, just as sway silently discards them.
    pub(in crate::layout) fn toggle_layout_cycle_for_target(
        &mut self,
        target: NodeKey,
        cycle: &[LayoutCycleEntry],
    ) -> bool {
        if cycle.is_empty() {
            return false;
        }

        self.flatten_layout_parent_once(target);
        let Some((source_key, current)) = self.toggle_source(target) else {
            return false;
        };
        let matches_current = |entry: LayoutCycleEntry| match entry {
            LayoutCycleEntry::Split => matches!(current, Layout::SplitH | Layout::SplitV),
            LayoutCycleEntry::Layout(layout) => layout == current,
        };
        let start = cycle
            .iter()
            .position(|entry| matches_current(*entry))
            .map_or(0, |idx| (idx + 1) % cycle.len());

        for offset in 0..cycle.len() {
            let next = match cycle[(start + offset) % cycle.len()] {
                LayoutCycleEntry::Split => self.toggled_split_layout(current, source_key),
                LayoutCycleEntry::Layout(layout) => layout,
            };
            if next != current {
                return self.apply_toggled_layout(next, source_key, target);
            }
        }

        false
    }

    /// Write back the layout a toggle arrived at.
    ///
    /// The container it read from is the container the command is about, so an ordinary one
    /// just changes type. A branch root is the exception, and not because it is special: a
    /// toggle that reads from it has either the root itself selected, which changes its
    /// orientation, or a window sitting on it, which gets the container sway builds instead.
    /// Telling those two apart is `set_layout_for_target`'s job, so they go there.
    fn apply_toggled_layout(
        &mut self,
        layout: Layout,
        source_key: Option<NodeKey>,
        target: NodeKey,
    ) -> bool {
        match source_key {
            Some(key) if self.parent_of(key).is_some() => self
                .get_real_container_mut(key)
                .map(|container| container.set_layout_explicit_from_command(layout))
                .is_some(),
            _ => self.set_layout_for_prepared_target(layout, target),
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
                    .and_then(|container| container.prev_split_layout())
                    .unwrap_or(along_the_screen)
            }
        }
    }

    /// The container a layout toggle reads its current layout from, and that layout.
    ///
    /// A leaf sitting directly on the workspace has no container between it and the
    /// workspace, so the toggle reads the workspace's own orientation — and its memory of
    /// the split it used to be, which is the same field on the same node as anywhere else.
    fn toggle_source(&self, target: NodeKey) -> Option<(Option<NodeKey>, Layout)> {
        let key = self.layout_container_key_for_target(target);
        match key.and_then(|key| self.get_container(key)) {
            Some(container) => Some((key, container.layout())),
            None => {
                if self.branch_is_addressable(self.branch_root(target)) {
                    return None;
                }
                Some((Some(self.root), self.root_container_layout()))
            }
        }
    }

    /// Layout of the container that owns `key` — or `key`'s own, when it is a branch root.
    pub(in crate::layout) fn layout_owning(&self, key: NodeKey) -> Option<Layout> {
        let container_key = self.parent_of(key).unwrap_or(key);
        self.get_container(container_key).map(|c| c.layout())
    }

    /// Layout of the container that currently owns the focused leaf (if any).
    /// Whether the container holding `key` should accept new splits.
    pub(in crate::layout) fn container_of_allows_splits(&self, key: NodeKey) -> bool {
        let Some(container_key) = self.parent_of(key) else {
            return false;
        };

        let Some(container) = self.get_real_container(container_key) else {
            return false;
        };
        container.child_count() > 1 || container.is_user_container()
    }

    /// Whether the focused container should accept new splits.
    pub(in crate::layout) fn focused_container_allows_splits(&self) -> bool {
        let Some(focused_key) = self.effective_focused_key() else {
            return false;
        };
        self.container_of_allows_splits(focused_key)
    }
}
