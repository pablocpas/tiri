//! Focus and selection: state, queries and directional navigation.

use super::ContainerTree;
use super::Direction;
use super::Layout;
use super::LayoutElement;
use super::NodeData;
use super::NodeKey;

impl<W: LayoutElement> ContainerTree<W> {
    pub(in crate::layout) fn selected_container_is_root(&self) -> bool {
        self.selected_container_key()
            .is_some_and(|selected_key| selected_key == self.root)
    }

    /// Whether focus resolves to the tree root itself rather than to a node inside it.
    pub(in crate::layout) fn focus_is_root(&self) -> bool {
        match self.effective_focused_key() {
            Some(key) => self.parent_of(key).is_none(),
            None => true,
        }
    }

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
        match self.focused_key {
            Some(key) if self.get_node(key).is_some() => Some(key),
            _ => self.first_leaf_key(),
        }
    }

    /// Current focus path within the tree.
    /// Uses cached path when generation and focused_key haven't changed.
    pub(in crate::layout) fn focus_path(&self) -> Vec<usize> {
        {
            let cache = self.focus_path_cache.borrow();
            if cache.0 == self.generation && cache.1 == self.focused_key {
                if let Some(key) = self.focused_key {
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
        cache.1 = self.focused_key;
        cache.2 = path.clone();
        path
    }

    pub(in crate::layout) fn selected_path(&self) -> Vec<usize> {
        if let Some(key) = self.selected_key {
            if let Some(path) = self.find_node_path(key) {
                return path;
            }
        }
        self.focus_path()
    }

    pub(in crate::layout) fn selected_node_key(&self) -> Option<NodeKey> {
        if let Some(key) = self.selected_key {
            if self.get_node(key).is_some() {
                return Some(key);
            }
        }
        self.focused_key.or_else(|| self.first_leaf_key())
    }

    /// Make `key` what a command with no explicit target acts on.
    pub(in crate::layout) fn select_node_key(&mut self, key: NodeKey) {
        self.selected_key = Some(key);
    }

    pub(in crate::layout) fn focused_node_key(&self) -> Option<NodeKey> {
        self.focused_key
    }

    pub(in crate::layout) fn root_node_key(&self) -> Option<NodeKey> {
        Some(self.root)
    }

    pub(in crate::layout) fn selected_is_container(&self) -> bool {
        self.selected_key
            .is_some_and(|key| matches!(self.get_node(key), Some(NodeData::Container(_))))
    }

    pub(in crate::layout) fn selected_container_key(&self) -> Option<NodeKey> {
        let key = self.selected_key?;
        matches!(self.get_node(key), Some(NodeData::Container(_))).then_some(key)
    }

    /// Select the container at `key` as the command target.
    pub(in crate::layout) fn select_container(&mut self, key: NodeKey) -> bool {
        if matches!(self.get_node(key), Some(NodeData::Container(_))) {
            self.selected_key = Some(key);
            true
        } else {
            false
        }
    }

    pub(in crate::layout) fn clear_selection(&mut self) {
        self.selected_key = None;
    }

    pub(in crate::layout) fn select_root_container(&mut self) -> bool {
        let root_key = self.root;
        if matches!(self.get_node(root_key), Some(NodeData::Container(_))) {
            self.selected_key = Some(root_key);
            true
        } else {
            false
        }
    }

    pub(in crate::layout) fn select_parent(&mut self) -> bool {
        let base_key = self
            .selected_key
            .or(self.focused_key)
            .or_else(|| self.first_leaf_key());
        let Some(base_key) = base_key else {
            return false;
        };
        let Some(parent_key) = self.parent_of(base_key) else {
            return false;
        };
        self.selected_key = Some(parent_key);
        true
    }

    pub(in crate::layout) fn select_child(&mut self) -> bool {
        let Some(selected_key) = self.selected_key else {
            return false;
        };
        let Some(container) = self.get_container(selected_key) else {
            return false;
        };
        let Some(child_key) = container.focused_child_key() else {
            return false;
        };
        self.selected_key = Some(child_key);
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

    pub(super) fn focus_in_direction_internal(
        &mut self,
        direction: Direction,
        allow_wrap: bool,
    ) -> bool {
        self.clear_focus_history();
        if self.is_empty() {
            return false;
        }

        if let Some(key) = self.focused_key {
            self.sync_container_focus_from_key(key);
        }

        let Some(selected_key) = self.selected_node_key() else {
            return false;
        };
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
            // if no direct movement was possible at this or any ancestor.
            let child_count = container.child_count();
            if allow_wrap
                && wrap_candidate.is_none()
                && child_count > 1
                && matches!(container.layout, Layout::SplitH | Layout::SplitV)
            {
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
                    self.focus_node_key(target_key);
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
                self.focus_node_key(target_key);
                return true;
            }
        }

        false
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
        let Some(focused_key) = self.focused_key else {
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
        let Some(focused_key) = self.focused_key else {
            return false;
        };
        let Some(parent_key) = self.parent_of(focused_key) else {
            return false;
        };
        let Some(parent) = self.get_container(parent_key) else {
            return false;
        };
        let Some(child_key) = parent.focused_child_key() else {
            return false;
        };
        self.focus_node_key(child_key);
        true
    }

    pub(super) fn prune_selected_key(&mut self) {
        if let Some(key) = self.selected_key {
            if self.get_node(key).is_none() {
                self.selected_key = None;
            }
        }
    }

    pub(super) fn reconcile_focus_after_change(&mut self, focused_removed: bool) {
        if self.is_empty() {
            self.focused_key = None;
        } else if focused_removed {
            self.focused_key = None;
            self.focus_first_leaf();
        } else if let Some(key) = self.focused_key {
            self.sync_container_focus_from_key(key);
        } else {
            self.focus_first_leaf();
        }
    }

    pub(super) fn focus_first_leaf(&mut self) {
        if let Some(key) = self.first_leaf_key() {
            self.focus_node_key(key);
        } else {
            self.focused_key = None;
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
        if let Some(focused) = self.focused_key {
            self.sync_container_focus_from_key(focused);
        } else {
            self.focus_first_leaf();
        }
    }
}
