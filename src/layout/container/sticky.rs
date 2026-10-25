//! Sticky floating containers.
//!
//! sway keeps sticky as a flag on the container (`is_sticky`) and gives it meaning in one
//! place: a floating root whose flag is set follows its output's visible workspace. The
//! container lives on that workspace's floating list like any other float, so it is laid out,
//! focused, found and reported by the same code; switching workspace moves it across with
//! `container_detach` + `workspace_add_floating`.
//!
//! sway/tree/container.c:1648-1654, sway/input/seat.c:1209-1221

use super::{ContainerTree, LayoutElement, NodeKey};

impl<W: LayoutElement> ContainerTree<W> {
    /// The raw flag, as `sticky enable|disable|toggle` left it and as IPC reports it.
    pub(in crate::layout) fn sticky_flag(&self, key: NodeKey) -> bool {
        self.get_any_container(key)
            .is_some_and(|container| container.sticky_flag())
    }

    /// Set the raw flag. Returns false for the workspace node or a key the tree does not hold.
    pub(in crate::layout) fn set_sticky_flag(&mut self, key: NodeKey, sticky: bool) -> bool {
        let Some(container) = self.get_any_container_mut(key) else {
            return false;
        };
        container.set_sticky_flag(sticky);
        true
    }

    /// sway's `container_is_sticky`: the flag only counts on a floating root.
    pub(in crate::layout) fn is_sticky(&self, key: NodeKey) -> bool {
        self.sticky_flag(key) && self.is_floating_root(key)
    }

    /// sway's `container_is_sticky_or_child`: whether the floating root above `key` is sticky.
    pub(in crate::layout) fn is_sticky_or_child(&self, key: NodeKey) -> bool {
        self.holds_node(key) && self.is_sticky(self.branch_root(key))
    }

    /// One workspace's sticky floating roots, topmost first.
    pub(in crate::layout) fn sticky_roots(&self, workspace: NodeKey) -> Vec<NodeKey> {
        self.floating_roots(workspace)
            .filter(|root| self.sticky_flag(*root))
            .collect()
    }

    /// The negation of sway's `workspace_is_empty`: a tiled window, or a float that is not
    /// sticky. Sticky floats belong to the output rather than to whichever workspace is
    /// showing them. Whether anything at all is on the workspace is
    /// [`Self::workspace_has_windows`].
    ///
    /// sway/tree/workspace.c:753-764
    pub(in crate::layout) fn workspace_has_non_sticky_windows(&self, workspace: NodeKey) -> bool {
        self.first_leaf_key(workspace).is_some()
            || self
                .floating_roots(workspace)
                .any(|root| !self.sticky_flag(root) && !self.leaf_keys_in_branch(root).is_empty())
    }

    /// Move a floating root to another workspace's floating list, keeping its record.
    ///
    /// sway's `container_detach` followed by `workspace_add_floating`: the node, its geometry
    /// and its place in the seat's order are unchanged; only the list holding it is. It lands
    /// at `stack_index` in the destination's top-to-bottom order.
    ///
    /// A floating root carries its position relative to the working area it was placed in,
    /// so it is re-anchored to the destination's working area, which is the same box when
    /// both workspaces are on one output.
    pub(in crate::layout) fn move_floating_root_to_workspace(
        &mut self,
        key: NodeKey,
        destination: NodeKey,
        stack_index: usize,
    ) -> bool {
        if !self.is_workspace(destination) {
            return false;
        }
        let Some((source, _)) = self.floating_slot_of(key) else {
            return false;
        };
        if source == destination {
            return false;
        }

        let (_, _, root) = self
            .unregister_floating_root(key)
            .expect("a floating slot names its ownership record");
        if self
            .fullscreen_key(source)
            .is_some_and(|scope| self.is_descendant_of(scope, key))
        {
            self.workspace_data_mut(source).fullscreen_key = None;
        }

        let working_area = self.working_area(destination);
        let roots = self
            .floating_entries_mut(destination)
            .expect("the destination was checked to be a workspace");
        roots.insert(stack_index.min(roots.len()), root);
        self.set_parent(key, Some(destination));
        self.adopt_tile_config(destination, key);
        let new_area = self.update_floating_working_area(key, working_area);
        let old_area = self.node_geometry(key).unwrap_or(new_area);
        self.translate_subtree_geometry(key, new_area.loc - old_area.loc);

        self.sync_fullscreen_window(destination);
        self.layout(source);
        self.layout(destination);
        true
    }
}
