//! Removing views and parking tiled subtrees for synchronous reparenting.

#[cfg(test)]
use super::DetachedView;
#[cfg(test)]
use super::InsertParentInfo;
use super::{ContainerArena, LayoutElement, NodeKey};

impl<W: LayoutElement> ContainerArena<W> {
    /// Unlink a view from its workspace while keeping the materialized node in the arena.
    ///
    /// Internal transfers use this form and adopt the returned key directly. Final unmap calls
    /// [`Self::remove_window`], which takes the parked node back out as an owned view.
    /// Take a window out of the tree and hand back the view, for a test driving a transfer.
    ///
    /// Production parks instead: the node stays in the arena and returns by key, which is what
    /// keeps its identity and its marks across the move. This is those two steps for a caller
    /// that wants the view in hand, and it is the only reason the pair still exists.
    #[cfg(test)]
    pub(in crate::layout) fn remove_window(
        &mut self,
        workspace: NodeKey,
        window_id: &W::Id,
    ) -> Option<DetachedView<W>> {
        let key = self.park_window(workspace, window_id)?;
        self.take_parked_view(key)
    }

    pub(in crate::layout) fn park_window(
        &mut self,
        workspace: NodeKey,
        window_id: &W::Id,
    ) -> Option<NodeKey> {
        let node_key = self.window_key(window_id)?;
        if self.workspace_of(node_key) != Some(workspace) {
            return None;
        }
        let cleanup_key = self.parent_of(node_key);
        let was_focused = self.seat_focused_key() == Some(node_key);
        // Not "was the closed window inside the fullscreen scope": what decides whether the
        // workspace inherits the selection is where the *selection* was. See
        // `reconcile_focus_after_change`.
        let selection_was_below_fullscreen =
            match (self.selected_key(), self.fullscreen_key(workspace)) {
                (Some(selected), Some(scope)) => {
                    selected != scope && self.is_descendant(selected, scope)
                }
                _ => false,
            };
        let former_ancestors = cleanup_key
            .map(|parent| self.focus_chain(parent))
            .unwrap_or_default();

        if self.selected_key() == Some(node_key) {
            self.seat.redirect_selection(None);
        }

        // Detach from the parent's child list before dropping the node itself. The
        // workspace has no parent and is not a window, so nothing routes here for it.
        let parent_key = self.parent_of(node_key)?;
        if let Some(child_idx) = self.child_index(parent_key, node_key) {
            if let Some(container) = self.get_container_mut(parent_key) {
                container.remove_child(child_idx);
            }
        }
        self.set_parent(node_key, None);
        assert!(self.park_subtree(node_key));
        self.drop_subtree_layouts(node_key);

        if let Some(cleanup_key) = cleanup_key {
            self.reap_empty(cleanup_key);
            self.autotile_squash_lone_children(workspace);
        }
        // A fullscreen container can outlive its last child until its owning floating list
        // drops the now-empty root. The workspace pointer must stop naming it before this
        // operation arranges the workspace, though: sway clears `ws->fullscreen` as part of
        // destroying the last view, then exposes the tiled branch that had been held at 0x0.
        // Keeping the empty scope for even this arrange skips every surviving branch.
        if self
            .fullscreen_key(workspace)
            .is_some_and(|scope| scope == node_key || self.branch_is_empty(scope))
        {
            self.fullscreen_keys.remove(&workspace);
        }
        self.prune_leaf_layouts(workspace);

        self.prune_selected_key();
        self.reconcile_focus_after_change(
            workspace,
            was_focused,
            &former_ancestors,
            selection_was_below_fullscreen,
        );

        self.layout(workspace);

        Some(node_key)
    }

    /// Park the subtree rooted at `node_key`, returning it along with enough information
    /// to put it back where it was.
    #[cfg(test)]
    pub(in crate::layout) fn take_subtree_at(
        &mut self,
        node_key: NodeKey,
    ) -> Option<(NodeKey, Option<InsertParentInfo>)> {
        self.get_node(node_key)?;
        let branch_root = self.branch_root(node_key);
        let insert_info = self
            .branch_relative_path(node_key)
            .and_then(|path| self.insert_parent_info_for_path(branch_root, &path));

        let subtree = self.take_tiling_subtree(node_key)?;
        Some((subtree, insert_info))
    }

    /// Park the command target for `move container`, preserving the addressed subtree.
    ///
    /// With no selected parent this is the focused leaf, including under tabbed/stacked. A
    /// parent selected through `focus parent` is intentionally moved as a group. The workspace
    /// itself is not a container target and therefore cannot be detached here.
    pub(in crate::layout) fn take_command_target_subtree(
        &mut self,
        workspace: NodeKey,
    ) -> Option<NodeKey> {
        let key = self.command_target_in(workspace);
        if self.is_workspace(key) {
            return None;
        }
        self.take_tiling_subtree(key)
    }

    fn take_tiling_subtree(&mut self, node_key: NodeKey) -> Option<NodeKey> {
        let workspace = self.workspace_of(node_key)?;
        self.get_node(node_key)?;
        if self.is_workspace(node_key) || !self.is_in_tiled_branch(node_key) {
            return None;
        }

        // Any outstanding layout describes a branch that still contains the node being
        // transferred. It cannot govern either the source after detachment or the destination.
        self.discard_layout_superseded_by_transfer(workspace);

        let focused_in_subtree = self
            .seat_focused_key()
            .is_some_and(|key| self.is_descendant_of(key, node_key));
        let former_ancestors = self
            .parent_of(node_key)
            .map(|parent| self.focus_chain(parent))
            .unwrap_or_default();

        if let Some(selected_key) = self.selected_key() {
            if self.is_descendant_of(selected_key, node_key) {
                self.seat.redirect_selection(None);
            }
        }

        let parent_key = self.parent_of(node_key)?;
        if let Some(idx) = self.child_index(parent_key, node_key) {
            if let Some(container) = self.get_container_mut(parent_key) {
                container.remove_child(idx);
            }
        }
        self.set_parent(node_key, None);

        if self
            .fullscreen_key(workspace)
            .is_some_and(|fullscreen| self.is_descendant_of(fullscreen, node_key))
        {
            self.fullscreen_keys.remove(&workspace);
        }
        assert!(self.park_subtree(node_key));
        self.drop_subtree_layouts(node_key);
        self.reap_empty(parent_key);
        self.autotile_squash_lone_children(workspace);
        self.prune_leaf_layouts(workspace);

        self.prune_selected_key();
        self.reconcile_focus_after_change(workspace, focused_in_subtree, &former_ancestors, false);

        self.layout(workspace);

        Some(node_key)
    }

    /// Park and return the root-level child at the given index.
    pub(in crate::layout) fn take_root_child_subtree(
        &mut self,
        workspace: NodeKey,
        idx: usize,
    ) -> Option<NodeKey> {
        let child = self.get_container(workspace)?.child_key(idx)?;
        self.take_tiling_subtree(child)
    }
}
