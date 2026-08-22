//! Focus and selection: state, queries and directional navigation.

use super::ContainerArena;
use super::Direction;
use super::LayoutElement;
use super::NodeData;
use super::NodeKey;

impl<W: LayoutElement> ContainerArena<W> {
    pub(in crate::layout) fn focused_leaf_targets_workspace_layout(&self) -> bool {
        self.effective_focused_key()
            .and_then(|key| self.parent_of(key))
            .is_none_or(|parent| parent == self.root)
    }

    /// The leaf that is effectively focused.
    ///
    /// Falls back to the first leaf when nothing is focused or the focused key went stale,
    /// so callers see the same node [`Self::focus_path`] resolves to.
    pub(in crate::layout) fn effective_focused_key(&self) -> Option<NodeKey> {
        match self.focused_key() {
            Some(key) if self.get_node(key).is_some() => Some(key),
            _ => self.first_leaf_key(),
        }
    }

    /// Current focus path, derived for legacy inspection APIs.
    pub(in crate::layout) fn focus_path(&self) -> Vec<usize> {
        self.effective_focused_key()
            .and_then(|key| self.find_node_path(key))
            .unwrap_or_default()
    }

    pub(in crate::layout) fn selected_path(&self) -> Vec<usize> {
        if let Some(key) = self.selected_key() {
            if let Some(path) = self.find_node_path(key) {
                return path;
            }
        }
        self.focus_path()
    }

    pub(in crate::layout) fn selected_node_key(&self) -> Option<NodeKey> {
        if let Some(key) = self.selected_key() {
            if self.get_node(key).is_some() {
                return Some(key);
            }
        }
        self.focused_key().or_else(|| self.first_leaf_key())
    }

    pub(in crate::layout) fn workspace_is_selected(&self) -> bool {
        self.selected_key() == Some(self.root)
            || (self.is_empty() && self.floating_roots().next().is_none())
    }

    pub(in crate::layout) fn focused_node_key(&self) -> Option<NodeKey> {
        self.focused_key()
    }

    pub(in crate::layout) fn root_node_key(&self) -> Option<NodeKey> {
        Some(self.root)
    }

    pub(in crate::layout) fn selected_container_key(&self) -> Option<NodeKey> {
        let key = self.selected_key()?;
        // A split, not a view. sway's containers are one type and this question is still two:
        // `focus parent` stopping on the thing a window is in is what the callers mean, and a
        // window is not something a window is in.
        self.get_node(key)
            .is_some_and(|node| node.is_split())
            .then_some(key)
    }

    /// The selected node that lays out children, including the workspace itself.
    ///
    /// Command semantics still use [`Self::selected_container_key`] when they require a real
    /// container. Rendering uses this broader question so selecting the workspace through
    /// `focus parent` remains visible without pretending that it is a `ContainerData<W>`.
    pub(in crate::layout) fn selected_layout_parent_key(&self) -> Option<NodeKey> {
        let key = self.selected_key()?;
        self.get_node(key)
            .is_some_and(|node| node.is_split() || matches!(node, NodeData::Workspace(_)))
            .then_some(key)
    }

    /// Whether the node sway would put in the command handler lives in this subtree.
    ///
    /// A selected container is the command target even though keyboard focus remains on a
    /// descendant view. Falling back to that leaf while a container is selected changes the
    /// meaning of both `focus parent` and directional focus.
    pub(in crate::layout) fn command_position_is_in(&self, scope_root: NodeKey) -> bool {
        self.seat
            .node()
            .is_some_and(|key| self.is_descendant(key, scope_root))
    }

    /// sway's `seat_set_focus` on a container: it becomes the selection *and* the most recent
    /// thing focused, ancestry included.
    ///
    /// `focus parent` is a focus command like any other — sway does not have a separate
    /// "selected" notion, it just focuses a node that happens to be a container. Recording the
    /// selection without raising it left the order thinking the last thing focused in there
    /// was whatever window had been focused before, and every later descent answered with it.
    fn select_node(&mut self, key: NodeKey) {
        let chain = self.focus_chain(key);
        let leaf = if key == self.root {
            self.focused_key()
                .filter(|focused| self.get_node(*focused).is_some_and(|node| node.is_view()))
                .or_else(|| self.focus_inactive_anywhere())
        } else {
            self.leaf_under_key(key)
        };
        self.seat.select(&chain, key, leaf);
        self.refresh_focus_visibility();
    }

    /// Select the container at `key` as the command target.
    pub(in crate::layout) fn select_container(&mut self, key: NodeKey) -> bool {
        if matches!(self.get_node(key), Some(NodeData::Container(_))) {
            self.select_node(key);
            true
        } else {
            false
        }
    }

    /// sway's `seat_set_focus` on a node, whatever kind it is.
    pub(in crate::layout) fn focus_node(&mut self, key: NodeKey) {
        self.focus_node_key(key);
    }

    pub(in crate::layout) fn clear_selection(&mut self) {
        self.seat.redirect_selection(None);
    }

    pub(in crate::layout) fn select_root_container(&mut self) -> bool {
        let root_key = self.root;
        if matches!(self.get_node(root_key), Some(NodeData::Workspace(_))) {
            self.select_node(root_key);
            true
        } else {
            false
        }
    }

    pub(in crate::layout) fn select_parent(&mut self) -> bool {
        let base_key = self
            .selected_key()
            .or(self.focused_key())
            .or_else(|| self.first_leaf_key());
        let Some(base_key) = base_key else {
            return false;
        };
        let Some(parent_key) = self.parent_of(base_key) else {
            return false;
        };
        self.select_node(parent_key);
        true
    }

    /// The node a command aimed at one branch reads its position from.
    ///
    /// One seat holds one selection over the whole workspace, so a branch asking where it
    /// stands has to say so: the seat may be pointing at the other side entirely.
    pub(in crate::layout) fn branch_position(&self, branch_root: NodeKey) -> Option<NodeKey> {
        self.selected_key()
            .filter(|key| self.branch_root(*key) == branch_root)
            .or_else(|| {
                self.focused_key()
                    .filter(|key| self.branch_root(*key) == branch_root)
            })
            .or_else(|| self.focus_inactive_view_in_branch(branch_root))
    }

    /// `focus parent` inside one subtree. Its root is the boundary, so the walk stops there.
    ///
    /// This accepts both a whole layout branch and a nested workspace-fullscreen scope. Using
    /// `branch_position` here discarded a valid selected descendant of the latter because its
    /// owning branch is still the workspace rather than the fullscreen node.
    pub(in crate::layout) fn select_parent_in(&mut self, scope_root: NodeKey) -> bool {
        let in_scope = |key| self.is_descendant(key, scope_root);
        let Some(base_key) = self
            .selected_key()
            .filter(|key| in_scope(*key))
            .or_else(|| self.focused_key().filter(|key| in_scope(*key)))
            .or_else(|| self.focus_inactive_view(scope_root))
        else {
            return false;
        };
        let Some(parent_key) = self.parent_of(base_key) else {
            return false;
        };
        if !self.is_descendant(parent_key, scope_root) {
            return false;
        }
        self.select_node(parent_key);
        true
    }

    pub(in crate::layout) fn select_child(&mut self) -> bool {
        let Some(selected_key) = self.selected_key() else {
            return false;
        };
        if self.get_container(selected_key).is_none() {
            return false;
        }
        let Some(child_key) = self.active_child(selected_key) else {
            return false;
        };
        self.select_node(child_key);
        true
    }

    /// Move focus in a direction
    pub(in crate::layout) fn focus_in_direction(&mut self, direction: Direction) -> bool {
        self.focus_in_direction_internal(direction, true)
    }

    /// Move focus in a direction without wrapping at container boundaries.
    pub(in crate::layout) fn focus_in_direction_no_wrap(&mut self, direction: Direction) -> bool {
        self.focus_in_direction_internal(direction, false)
    }

    /// sway's `focus next|prev`: the direction is whichever way the parent lays its children
    /// out, and everything after that is an ordinary directional focus.
    ///
    /// `sibling` stops a direct step descending into the container it lands on. Sway's wrap
    /// fallback is deliberately different: it always resolves the wrapped-to container's
    /// inactive view, so wrapping descends even for `next|prev sibling`.
    pub(in crate::layout) fn focus_along_parent(&mut self, forward: bool, descend: bool) -> bool {
        let Some(selected_key) = self.selected_node_key() else {
            return false;
        };
        let Some(direction) = self.direction_along_parent(selected_key, forward) else {
            return false;
        };

        self.clear_focus_history();
        self.focus_in_direction_from_until(selected_key, direction, true, descend, None)
    }

    fn direction_along_parent(&self, key: NodeKey, forward: bool) -> Option<Direction> {
        let parent_layout = self
            .parent_of(key)
            .and_then(|parent_key| self.get_container(parent_key).map(|parent| parent.layout()))?;
        Some(match (parent_layout.is_horizontal(), forward) {
            (true, true) => Direction::Right,
            (true, false) => Direction::Left,
            (false, true) => Direction::Down,
            (false, false) => Direction::Up,
        })
    }

    pub(super) fn focus_in_direction_internal(
        &mut self,
        direction: Direction,
        allow_wrap: bool,
    ) -> bool {
        self.focus_in_direction_with(direction, allow_wrap, true)
    }

    fn focus_in_direction_with(
        &mut self,
        direction: Direction,
        allow_wrap: bool,
        descend: bool,
    ) -> bool {
        self.clear_focus_history();
        let Some(selected_key) = self.selected_node_key() else {
            return false;
        };
        self.focus_in_direction_from(selected_key, direction, allow_wrap, descend)
    }

    /// The same, inside one branch: the walk stops at its root, so focus never leaves it.
    pub(in crate::layout) fn focus_in_direction_in_branch(
        &mut self,
        branch_root: NodeKey,
        direction: Direction,
        allow_wrap: bool,
    ) -> bool {
        self.clear_focus_history();
        let Some(start) = self.branch_position(branch_root) else {
            return false;
        };
        self.focus_in_direction_from_until(start, direction, allow_wrap, true, Some(branch_root))
    }

    fn focus_in_direction_from(
        &mut self,
        selected_key: NodeKey,
        direction: Direction,
        allow_wrap: bool,
        descend: bool,
    ) -> bool {
        self.focus_in_direction_from_until(selected_key, direction, allow_wrap, descend, None)
    }

    fn focus_in_direction_from_until(
        &mut self,
        selected_key: NodeKey,
        direction: Direction,
        allow_wrap: bool,
        descend: bool,
        boundary: Option<NodeKey>,
    ) -> bool {
        let mut wrap_candidate: Option<(NodeKey, usize)> = None;

        // Walk ancestors from the innermost container outwards, trying a direct sibling
        // step at every level whose layout runs along `direction`. A branch operation stops
        // before considering the branch root's parent; crossing it would leave the branch.
        let mut current = selected_key;
        loop {
            // Sway tests fullscreen on the node currently being climbed, not as a global
            // scope chosen before the walk. Descendants may therefore move among themselves,
            // and an exterior sibling may enter a fullscreen container directly; only trying
            // to climb through the fullscreen owner stops the search (and suppresses wrap).
            //
            // The branch root is climbed *to* before the walk stops there, so it is tested
            // like any other node: `node_get_in_direction_tiling` reaches a fullscreen
            // floating root through `pending.parent` and leaves for the next output from
            // there, discarding the wrap candidate a child had recorded on the way up.
            if self.fullscreen_key == Some(current) {
                return false;
            }
            if boundary == Some(current) {
                break;
            }
            let Some(parent_key) = self.parent_of(current) else {
                break;
            };
            let Some(container) = self.get_container(parent_key) else {
                current = parent_key;
                continue;
            };
            if !container.layout.is_parallel_to(direction) {
                current = parent_key;
                continue;
            }
            let Some(current_idx) = self.child_index(parent_key, current) else {
                current = parent_key;
                continue;
            };

            // Remember a wrap candidate at the first matching container, but only use it
            // if no direct movement was possible at this or any ancestor. Every container
            // laid out along the direction is a candidate — a tabbed one wraps between its
            // tabs exactly as a split one wraps between its children.
            let child_count = container.child_count();
            if allow_wrap && wrap_candidate.is_none() && child_count > 1 {
                let wrap_idx = if direction.is_leading() {
                    child_count - 1
                } else {
                    0
                };
                wrap_candidate = Some((parent_key, wrap_idx));
            }

            // First try direct movement without wrapping.
            if let Some(new_idx) = direction.sibling_index(current_idx, child_count) {
                if let Some(target_key) = container.child_key(new_idx) {
                    self.focus_landing_on(target_key, descend);
                    return true;
                }
            }

            current = parent_key;
        }

        if let Some((container_key, wrap_idx)) = wrap_candidate {
            if let Some(target_key) = self
                .get_container(container_key)
                .and_then(|container| container.child_key(wrap_idx))
            {
                self.focus_landing_on(target_key, true);
                return true;
            }
        }

        false
    }

    /// Land on `key`: on the window inside it, or on the container itself when the focus was
    /// told not to descend.
    fn focus_landing_on(&mut self, key: NodeKey, descend: bool) {
        if !descend && matches!(self.get_node(key), Some(NodeData::Container(_))) {
            self.select_node(key);
            return;
        }
        // sway descends with `seat_get_focus_inactive_view`, not by position: the window a
        // container hands back is the one most recently focused inside it, however deep and
        // whatever its index. Walking down the active child of each level gives the same
        // answer whenever the seat order runs through this subtree, and a different one when
        // the order knows a window here that the walk does not reach.
        let landing = self.focus_inactive_view(key).unwrap_or(key);
        self.focus_node_key(landing);
    }

    /// sway's `seat_get_focus_inactive_view`: the most recently focused window under `key`.
    ///
    /// One question of the seat's order, where tiri used to ask each container in turn for
    /// its active child and follow the chain down. The two agree while the order runs through
    /// every level, and part ways exactly where it does not — a subtree the focus left by
    /// some route the per-level walk cannot retrace.
    pub(in crate::layout) fn focus_inactive_view(&self, key: NodeKey) -> Option<NodeKey> {
        if self.get_node(key).is_some_and(|node| node.is_view()) {
            return Some(key);
        }
        self.focus_inactive_view_from_order(key)
            .or_else(|| self.leaf_under_key(key))
    }

    /// The most recent view in one layout branch, excluding sibling floating branches when the
    /// requested branch is the workspace's tiled side.
    pub(in crate::layout) fn focus_inactive_view_in_branch(
        &self,
        branch_root: NodeKey,
    ) -> Option<NodeKey> {
        if self
            .get_node(branch_root)
            .is_some_and(|node| node.is_view())
        {
            return Some(branch_root);
        }
        self.seat
            .order()
            .iter()
            .copied()
            .find(|candidate| {
                self.get_node(*candidate).is_some_and(|node| node.is_view())
                    && self.branch_root(*candidate) == branch_root
            })
            .or_else(|| self.leaf_under_key(branch_root))
    }

    pub(super) fn focus_inactive_node_in_branch(&self, branch_root: NodeKey) -> Option<NodeKey> {
        self.seat.order().iter().copied().find(|candidate| {
            // A workspace is the owner of the tiling branch, not a node *under* that
            // branch.  sway's seat_get_focus_inactive_tiling() therefore skips it and can
            // recover the last real tiling container after focus moved to floating.
            *candidate != self.root
                && self.get_node(*candidate).is_some()
                && self.branch_root(*candidate) == branch_root
        })
    }

    fn focus_inactive_view_from_order(&self, key: NodeKey) -> Option<NodeKey> {
        self.seat.order().iter().copied().find(|candidate| {
            self.get_node(*candidate).is_some_and(|node| node.is_view())
                && self.is_descendant(*candidate, key)
        })
    }

    /// Apply the focus-stack side effect of destroying a node that did not own seat focus.
    ///
    /// sway's destroy listener raw-focuses the nearest surviving inactive view, then restores
    /// the workspace and the previous focus (`sway/input/seat.c:261-324`). Keyboard focus does
    /// not move, but the promoted sibling becomes the answer inside its branch. Omitting this
    /// is why a later wrapped `focus next sibling` descended to an older view.
    pub(super) fn unregister_unfocused_node(&mut self, key: NodeKey, surviving_parent: NodeKey) {
        let restore = self.seat.node();
        let was_focused = restore.is_some_and(|focused| {
            focused == key || (self.nodes.contains_key(focused) && self.is_descendant(focused, key))
        });
        self.seat.unregister(key);
        if was_focused {
            return;
        }

        let next = self.focus_inactive_view_from_order(surviving_parent);
        if let Some(next) = next {
            self.seat.raw_focus(next);
            self.seat.raw_focus(self.root);
            if let Some(restore) = restore.filter(|node| self.nodes.contains_key(*node)) {
                self.seat.raw_focus(restore);
            }
        }
    }

    /// Focus window by its ID if present.
    pub(in crate::layout) fn focus_window_by_id(&mut self, window_id: &W::Id) -> bool {
        self.clear_focus_history();
        let Some(key) = self.window_key(window_id) else {
            return false;
        };
        self.focus_node_key(key);
        true
    }

    #[cfg(test)]
    pub(in crate::layout) fn focus_parent(&mut self) -> bool {
        self.clear_focus_history();
        let Some(focused_key) = self.focused_key() else {
            return false;
        };
        let Some(parent_key) = self.parent_of(focused_key) else {
            return false;
        };
        self.focus_node_key(parent_key);
        true
    }

    #[cfg(test)]
    pub(in crate::layout) fn focus_child(&mut self) -> bool {
        self.clear_focus_history();
        let Some(focused_key) = self.focused_key() else {
            return false;
        };
        let Some(parent_key) = self.parent_of(focused_key) else {
            return false;
        };
        if self.get_container(parent_key).is_none() {
            return false;
        }
        let Some(child_key) = self.active_child(parent_key) else {
            return false;
        };
        self.focus_node_key(child_key);
        true
    }

    pub(super) fn prune_selected_key(&mut self) {
        if let Some(key) = self.selected_key() {
            if self.get_node(key).is_none() {
                self.seat.redirect_selection(None);
            }
        }
    }

    /// Where focus goes when what held it is gone.
    ///
    /// sway's node-destroy listener asks `seat_get_focus_inactive_view` of the removed node's
    /// parent first, then each ancestor in turn (`sway/input/seat.c:261-298`). Only when none
    /// of those can answer does it ask the last workspace. Going straight to the workspace
    /// made closing one child of a nested split jump to a more recently focused window in a
    /// different branch.
    ///
    /// The ancestor keys are captured before the removal because `container_reap_empty` may
    /// destroy one or more of them before tiri settles focus.
    pub(super) fn reconcile_focus_after_change(
        &mut self,
        focused_removed: bool,
        former_ancestors: &[NodeKey],
        selection_was_below_fullscreen: bool,
    ) {
        // The node that left stops answering for anything, including for the container it
        // was in.
        self.prune_focus_order();

        // Both sides: the seat belongs to the workspace, and a workspace with a floating
        // window in it is not one where nothing is focused.
        if self.dfs_leaf_keys().is_empty() {
            self.seat.clear();
            return;
        }

        let needs_new_focus = focused_removed
            || self
                .focused_key()
                .is_none_or(|key| !self.nodes.contains_key(key));
        if !needs_new_focus {
            return;
        }

        let nearest_surviving = former_ancestors
            .iter()
            .copied()
            .filter(|ancestor| self.nodes.contains_key(*ancestor))
            .find_map(|ancestor| self.focus_inactive_view(ancestor));

        // The last-workspace fallback still covers both lists: a floating window that was
        // focused more recently than anything tiled is what focus falls to once no surviving
        // ancestor of the removed node can answer.
        match nearest_surviving.or_else(|| self.focus_inactive_anywhere()) {
            Some(key)
                if self.selected_key().is_some_and(|selected| {
                    matches!(self.get_node(selected), Some(NodeData::Container(_)))
                        && self.leaf_under_key(selected).is_some()
                }) || selection_was_below_fullscreen =>
            {
                // A selected container that survives keeps owning focus while its inactive
                // view is replaced. Fullscreen destruction is sway's exceptional inherited
                // workspace selection: ordinary container destruction descends to the
                // surviving sibling instead of making the next `close` target the workspace.
                //
                // Surviving means what sway means by it. `container_reap_empty` destroys a
                // container the moment its last child leaves, so a selection sway would have
                // dropped cannot go on redirecting focus here — and redirecting is all this
                // arm does, leaving the seat's order pointing at the emptied branch. An
                // emptied floating root outlives this call because the floating list drops
                // it, which is exactly the case that needs saying so.
                //
                // The second condition is the workspace one, and it is about where the
                // *selection* was rather than where the closed window was. A selection
                // strictly below a fullscreen node loses every ancestor at once and comes
                // back up at the workspace, which is what stays selected; a selection that
                // *is* the fullscreen node loses only itself, and focus descends to the
                // window that was waiting behind it. Both are recorded:
                // `close-selected-fullscreen-container-keeps-parent-selection` and
                // `close-the-last-window-of-a-selected-fullscreen-split`.
                self.seat.redirect_focused_leaf(Some(key));
            }
            Some(key) => self.focus_node_key(key),
            None => self.focus_first_leaf(),
        }
    }

    /// The most recently focused window anywhere in the workspace, either side.
    pub(in crate::layout) fn focus_inactive_anywhere(&self) -> Option<NodeKey> {
        let root = self.root;
        self.seat
            .order()
            .iter()
            .copied()
            .find(|candidate| {
                self.get_node(*candidate).is_some_and(|node| node.is_view())
                    && self.nodes.contains_key(*candidate)
            })
            .or_else(|| self.focus_inactive_view(root))
            // The seat's order is emptied by an insert before the new window is placed, so
            // when the workspace's only window is floating there is nothing in it to find and
            // nothing under the tiled root either. A floating window is still a window this
            // workspace can answer with.
            .or_else(|| self.dfs_leaf_keys().first().copied())
    }

    /// The most recently focused floating window in this workspace.
    ///
    /// This is another filtered read of the one seat order, not a floating-side MRU. A mode
    /// switch must not need the layout-wide focus history to rediscover state already owned
    /// by the workspace.
    pub(in crate::layout) fn inactive_floating_window_id(&self) -> Option<W::Id> {
        self.seat.order().iter().find_map(|key| {
            self.is_in_floating_branch(*key)
                .then(|| self.get_tile(*key))
                .flatten()
                .map(|tile| tile.window().id().clone())
        })
    }

    /// The floating view commands and rendering should treat as active.
    ///
    /// When keyboard focus is currently in the floating side, the focused leaf wins even for
    /// `follow_without_raising`: focus and stacking are deliberately independent. When tiling is
    /// active, filter the same seat order to recover the most recently focused floating view.
    /// There is no floating-side focus cache to reconcile with either answer.
    pub(in crate::layout) fn active_floating_window_id(&self) -> Option<W::Id> {
        self.focused_key()
            .filter(|key| self.is_in_floating_branch(*key))
            .and_then(|key| self.get_tile(key))
            .map(|tile| tile.window().id().clone())
            .or_else(|| self.inactive_floating_window_id())
    }

    /// Focus what this workspace answers with when nothing has said otherwise.
    ///
    /// The tiled side first, because that is where a workspace's first window goes. Then any
    /// leaf at all: a workspace whose only window is floating is not one where nothing is
    /// focused, and `seat_get_focus_inactive` has to answer for it too. Only an empty
    /// workspace clears the seat.
    ///
    /// The tiled-only version was right while the floating side was a tree of its own — an
    /// empty tiling tree really did mean an empty workspace. It is the caller of last resort
    /// for every path that loses its focused node, so it was the one place that had to learn
    /// there are two sides.
    ///
    /// sway/input/seat.c:1415
    pub(super) fn focus_first_leaf(&mut self) {
        let key = self
            .first_leaf_key()
            .or_else(|| self.dfs_leaf_keys().first().copied());
        if let Some(key) = key {
            self.focus_node_key(key);
        } else {
            self.seat.clear();
        }
    }

    /// Settle focus after inserting `key`: focus it when requested, otherwise keep the
    /// current focus chain intact (falling back to the first leaf if nothing is focused).
    pub(super) fn settle_focus_after_insert(&mut self, key: NodeKey, focus: bool) {
        if focus {
            self.focus_node_key(key);
        } else {
            self.resync_focus();
        }
    }

    /// Re-derive the per-container focus chain from the focused leaf, falling back to the
    /// most recently focused window in the workspace when nothing is focused.
    ///
    /// Both sides. `focus_first_leaf` walks from the workspace root, which is the tiled side
    /// alone, and a workspace whose only window is floating would come out of an insert with
    /// nothing focused — `seat_get_focus_inactive` has no answer for it, and every descent
    /// into that workspace asks. It was correct while the floating side was a tree of its own
    /// and an empty tiling tree really did mean an empty workspace. One arena, one answer.
    ///
    /// sway/input/seat.c:1415
    pub(super) fn resync_focus(&mut self) {
        if self.focused_key().is_some() {
            return;
        }
        match self.focus_inactive_anywhere() {
            Some(key) => self.focus_node_key(key),
            None => self.focus_first_leaf(),
        }
    }
}

impl<W: LayoutElement> ContainerArena<W> {
    /// A node and its ancestors, the node first — the order sway adds them to the seat in.
    pub(super) fn focus_chain(&self, key: NodeKey) -> Vec<NodeKey> {
        let mut chain = Vec::new();
        let mut current = Some(key);
        while let Some(node) = current {
            chain.push(node);
            current = self.parent_of(node);
        }
        chain
    }

    pub(in crate::layout) fn focused_key(&self) -> Option<NodeKey> {
        self.seat.focused_leaf()
    }

    pub(in crate::layout) fn selected_key(&self) -> Option<NodeKey> {
        self.seat.selected()
    }

    /// sway's `seat_get_active_tiling_child`: which child of `parent` a switcher shows.
    ///
    /// The first entry in the seat's focus order whose *direct parent* is `parent`. Not a
    /// descendant — a child. That single word is the whole of the rule: a node moved deeper
    /// into a switcher stops answering for the one it left, without the move touching any
    /// focus state at all.
    ///
    /// At the workspace the search skips floats, as sway's does: the children it walks are
    /// `ws->tiling`, and a floating node is in `ws->floating` instead. One arena holds both
    /// here, so the same distinction is a question about the branch a child roots.
    pub(in crate::layout) fn active_tiling_child(&self, parent: NodeKey) -> Option<NodeKey> {
        self.seat.order().iter().copied().find(|key| {
            self.parent_of(*key) == Some(parent)
                && (parent != self.root || self.branch_root(*key) == self.root)
        })
    }

    /// Which child of `parent` is the active one, the only question either order is asked.
    ///
    /// [`Self::active_tiling_child`] with the fallback sway gets for free: a container
    /// nothing inside has ever been focused still shows something, and what it shows is its
    /// first child. sway never reaches that state — every container it builds is built
    /// around a node the seat already knows — so it has no rule for it and neither is this
    /// one; it is where tiri builds containers before anything has been focused into them.
    pub(in crate::layout) fn active_child(&self, parent: NodeKey) -> Option<NodeKey> {
        self.active_tiling_child(parent).or_else(|| {
            self.get_container(parent)?
                .children()
                .iter()
                .copied()
                .find(|key| parent != self.root || self.branch_root(*key) == self.root)
        })
    }

    /// Whether `key` is the child its parent would descend to — sway's
    /// `seat_get_focus_inactive` answer, asked one level up.
    ///
    /// This is the whole of the `focused_inactive` decoration state. sway renders a level at
    /// a time (`render_container_simple`) and compares each child against the focus-inactive
    /// child of *that* level, so the question is local: a node is `focused_inactive` when its
    /// own parent would come back to it, whatever is happening above the parent. The focus
    /// head of a container nested inside a container nobody has focused still shows it,
    /// because its parent still points at it.
    ///
    /// It is deliberately not "is anything in here focused": the globally focused leaf is
    /// also its parent's active child, and callers pick `Focused` for it first.
    pub(in crate::layout) fn is_focus_head(&self, key: NodeKey) -> bool {
        let Some(parent) = self.parent_of(key) else {
            return false;
        };
        self.active_child(parent) == Some(key)
    }

    /// Where [`Self::active_child`] sits in its parent's child list.
    pub(in crate::layout) fn active_child_index(&self, parent: NodeKey) -> Option<usize> {
        let key = self.active_child(parent)?;
        self.child_index(parent, key)
    }

    /// Put a node at the head of the seat's focus order, and every ancestor with it.
    ///
    /// sway's `seat_set_focus` walks up from the focused node adding each ancestor, so a
    /// container is ahead of its siblings exactly when something inside it was focused more
    /// recently than anything inside them.
    ///
    /// Drop nodes the tree no longer holds.
    pub(in crate::layout) fn prune_focus_order(&mut self) {
        self.seat.prune(&self.nodes);
    }
}
