//! Focus and selection: state, queries and directional navigation.

use super::ContainerData;
use super::ContainerTree;
use super::Direction;
use super::LayoutElement;
use super::NodeData;
use super::NodeKey;

impl<W: LayoutElement> ContainerTree<W> {
    pub(in crate::layout) fn focused_leaf_targets_workspace_layout(&self) -> bool {
        let focus_path = self.focus_path();
        focus_path.is_empty()
            || (focus_path.len() == 1 && self.root_is_synthetic_workspace_container())
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

    /// Current focus path within the tree.
    /// Uses cached path when generation and focused_key haven't changed.
    pub(in crate::layout) fn focus_path(&self) -> Vec<usize> {
        {
            let cache = self.focus_path_cache.borrow();
            if cache.0 == self.generation && cache.1 == self.focused_key() {
                if let Some(key) = self.focused_key() {
                    if self.get_node_key_at_path(&cache.2) == Some(key) {
                        return cache.2.clone();
                    }
                }
            }
        }

        // Recompute path with fallback when focused key is invalid.
        let path = self
            .effective_focused_key()
            .and_then(|key| self.find_node_path(key))
            .unwrap_or_default();

        // Update cache
        let mut cache = self.focus_path_cache.borrow_mut();
        cache.0 = self.generation;
        cache.1 = self.focused_key();
        cache.2 = path.clone();
        path
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

    pub(in crate::layout) fn focused_node_key(&self) -> Option<NodeKey> {
        self.focused_key()
    }

    pub(in crate::layout) fn root_node_key(&self) -> Option<NodeKey> {
        Some(self.root)
    }

    pub(in crate::layout) fn selected_container_key(&self) -> Option<NodeKey> {
        let key = self.selected_key()?;
        matches!(self.get_node(key), Some(NodeData::Container(_))).then_some(key)
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
        let leaf = self.leaf_under_key(key);
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
        if matches!(self.get_node(root_key), Some(NodeData::Container(_))) {
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
            .filter(|key| self.is_descendant(*key, branch_root))
            .or_else(|| {
                self.focused_key()
                    .filter(|key| self.is_descendant(*key, branch_root))
            })
            .or_else(|| self.focus_inactive_view(branch_root))
    }

    /// `focus parent` inside one branch. Its root has no parent, so the walk stops there.
    pub(in crate::layout) fn select_parent_in(&mut self, branch_root: NodeKey) -> bool {
        let Some(base_key) = self.branch_position(branch_root) else {
            return false;
        };
        let Some(parent_key) = self.parent_of(base_key) else {
            return false;
        };
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
        let Some(parent_layout) = self
            .parent_of(selected_key)
            .and_then(|parent_key| self.get_container(parent_key).map(ContainerData::layout))
        else {
            return false;
        };

        let direction = match (parent_layout.is_horizontal(), forward) {
            (true, true) => Direction::Right,
            (true, false) => Direction::Left,
            (false, true) => Direction::Down,
            (false, false) => Direction::Up,
        };
        self.focus_in_direction_with(direction, true, descend)
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
        if self.is_empty() {
            return false;
        }

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
        self.focus_in_direction_from(start, direction, allow_wrap, true)
    }

    fn focus_in_direction_from(
        &mut self,
        selected_key: NodeKey,
        direction: Direction,
        allow_wrap: bool,
        descend: bool,
    ) -> bool {
        let mut wrap_candidate: Option<(NodeKey, usize)> = None;

        // Walk ancestors from the innermost container outwards, trying a direct sibling
        // step at every level whose layout runs along `direction`.
        let mut current = selected_key;
        while let Some(parent_key) = self.parent_of(current) {
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
        if matches!(self.get_node(key), Some(NodeData::Leaf(_))) {
            return Some(key);
        }
        self.focus_inactive_view_from_order(key)
            .or_else(|| self.leaf_under_key(key))
    }

    fn focus_inactive_view_from_order(&self, key: NodeKey) -> Option<NodeKey> {
        self.seat.order().iter().copied().find(|candidate| {
            matches!(self.get_node(*candidate), Some(NodeData::Leaf(_)))
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
                matches!(self.get_node(*candidate), Some(NodeData::Leaf(_)))
                    && self.nodes.contains_key(*candidate)
            })
            .or_else(|| self.focus_inactive_view(root))
    }

    pub(super) fn focus_first_leaf(&mut self) {
        if let Some(key) = self.first_leaf_key() {
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
    /// first leaf when nothing is focused.
    pub(super) fn resync_focus(&mut self) {
        if self.focused_key().is_none() {
            self.focus_first_leaf();
        }
    }
}

impl<W: LayoutElement> ContainerTree<W> {
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
    pub(in crate::layout) fn active_tiling_child(&self, parent: NodeKey) -> Option<NodeKey> {
        self.seat
            .order()
            .iter()
            .copied()
            .find(|key| self.parent_of(*key) == Some(parent))
    }

    /// Which child of `parent` is the active one, the only question either order is asked.
    ///
    /// [`Self::active_tiling_child`] with the fallback sway gets for free: a container
    /// nothing inside has ever been focused still shows something, and what it shows is its
    /// first child. sway never reaches that state — every container it builds is built
    /// around a node the seat already knows — so it has no rule for it and neither is this
    /// one; it is where tiri builds containers before anything has been focused into them.
    pub(in crate::layout) fn active_child(&self, parent: NodeKey) -> Option<NodeKey> {
        self.active_tiling_child(parent)
            .or_else(|| self.get_container(parent)?.children().first().copied())
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

    /// Drop nodes the tree no longer holds.
    pub(in crate::layout) fn prune_focus_order(&mut self) {
        self.seat.prune(&self.nodes);
    }
}
