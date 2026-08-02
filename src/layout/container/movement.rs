//! Moving a node in a direction.
//!
//! A port of sway's `container_move_in_direction` and the two normalizations `cmd_move`
//! runs around it: `container_reap_empty` on the parent the node left, and
//! `workspace_squash` over the workspace. Nothing else — sway normalizes per command rather
//! than by a general walk, and which normalization runs where is most of the behaviour.
//!
//! Every rule that used to live here was an approximation of one of those: climb to the
//! outermost parallel ancestor, give up when a window is alone all the way up, wrap the
//! workspace's children by hand at the top level. Each disagreed with sway somewhere, and
//! the disagreements did not share a cause, which is what made them look like eight bugs.

use super::ContainerData;
use super::ContainerTree;
use super::Direction;
use super::Layout;
use super::LayoutElement;
use super::NodeData;
use super::NodeKey;
#[cfg(test)]
use super::RootPolicy;
use super::TreeCommandTarget;

impl<W: LayoutElement> ContainerTree<W> {
    /// Move the current command target in a direction.
    #[cfg(test)]
    pub(in crate::layout) fn move_in_direction(&mut self, direction: Direction) -> bool {
        self.move_target_in_direction(
            direction,
            self.command_target(RootPolicy::MaterialContainer),
        )
    }

    /// Move an explicit command target in a direction.
    ///
    /// sway's `cmd_move_container`: the move itself, then the parent the node left is reaped
    /// if the node was the last thing in it.
    pub(in crate::layout) fn move_target_in_direction(
        &mut self,
        direction: Direction,
        target: TreeCommandTarget,
    ) -> bool {
        self.clear_focus_history();
        if self.is_empty() {
            return false;
        }

        let Some((node_key, preserve_selected_container)) = self.resolve_move_source(target) else {
            return false;
        };

        // Both read before the move: the node keeps its identity, but the squash afterwards
        // can splice it away, and the parent it leaves may not survive being emptied.
        let old_parent_key = self.parent_of(node_key);
        let leaf_key = self.leaf_under_key(node_key);

        if !self.move_node(node_key, direction) {
            return false;
        }

        if let Some(old_parent_key) = old_parent_key {
            self.reap_empty(old_parent_key);
        }
        if let Some(leaf_key) = leaf_key {
            self.focus_node_key(leaf_key);
        }
        if preserve_selected_container && self.nodes.contains_key(node_key) {
            self.selected_key = Some(node_key);
        }
        true
    }

    /// Resolve the command target into the node to move, and whether it is a container whose
    /// selection should survive the move.
    fn resolve_move_source(&mut self, target: TreeCommandTarget) -> Option<(NodeKey, bool)> {
        let (move_key, preserve_selected_container) = match target {
            TreeCommandTarget::Workspace => return None,
            TreeCommandTarget::Container(key) => {
                matches!(self.get_node(key), Some(NodeData::Container(_))).then_some((key, true))?
            }
            TreeCommandTarget::Leaf(key) => {
                matches!(self.get_node(key), Some(NodeData::Leaf(_))).then_some((key, false))?
            }
        };

        self.sync_container_focus_from_key(move_key);

        // The workspace has nowhere to go, and moving it is not one of these commands.
        (move_key != self.root).then_some((move_key, preserve_selected_container))
    }

    /// sway's `container_move_in_direction`.
    ///
    /// Climb until an ancestor sits in a parent laid out along the direction. If there is a
    /// neighbour on that side the node moves in with it; if there is not, the node is
    /// promoted to sit beside that ancestor. A workspace facing the wrong way is turned on
    /// the way up, its children moving under one container that keeps the old orientation.
    fn move_node(&mut self, node_key: NodeKey, direction: Direction) -> bool {
        let mut current = node_key;
        let mut wrapped = false;

        let (ancestor_key, ancestor_idx) = loop {
            let Some(parent_key) = self.parent_of(current) else {
                return false;
            };
            let Some(parent) = self.get_container(parent_key) else {
                return false;
            };
            let parent_layout = parent.layout();
            let child_count = parent.child_count();

            if !parent_layout.is_parallel_to(direction) {
                if parent_key != self.root {
                    // Keep looking for a parallel parent.
                    current = parent_key;
                    continue;
                }
                // Nothing anywhere faces the right way, so the workspace turns to face it.
                let Some(wrapper_key) =
                    self.wrap_workspace_children(parent_layout, direction.split_layout())
                else {
                    return false;
                };
                current = wrapper_key;
                wrapped = true;
                continue;
            }

            let Some(current_idx) = self.child_index(parent_key, current) else {
                return false;
            };
            let target_key = direction
                .sibling_index(current_idx, child_count)
                .and_then(|idx| self.get_container(parent_key)?.child_key(idx));

            if let Some(target_key) = target_key {
                // Either the node's own neighbour or a cousin found further up, and the same
                // thing happens to both: swap with it, or move in with it.
                self.move_into_from_direction(node_key, target_key, direction);
                return true;
            }

            if current != node_key {
                break (current, current_idx);
            }

            if parent_key == self.root {
                // The node is at workspace level with nothing beside it, which is where sway
                // hands it to the next output. There is one workspace here.
                return false;
            }

            // The node has escaped its immediate parallel parent; carry on above it.
            current = parent_key;
        };

        let Some(node_parent_key) = self.parent_of(node_key) else {
            return false;
        };

        // sway treats a lone child of the workspace as if it were at workspace level and
        // hands it to the next output, so the move stops here. Not when the workspace has
        // just been turned: that is a change in itself.
        let node_parent_is_lone_top_level = self.parent_of(node_parent_key) == Some(self.root)
            && self
                .get_container(node_parent_key)
                .is_some_and(|parent| parent.child_count() == 1);
        if !wrapped && node_parent_is_lone_top_level {
            return false;
        }

        // Beside the ancestor it came out of, on the side the move points to.
        let Some(ancestor_parent_key) = self.parent_of(ancestor_key) else {
            return false;
        };
        let insert_at = if direction.is_leading() {
            ancestor_idx
        } else {
            ancestor_idx + 1
        };
        self.reparent(node_key, ancestor_parent_key, insert_at);

        self.reap_empty(node_parent_key);
        self.squash_workspace();
        true
    }

    /// sway's `container_move_to_container_from_direction`: put `node_key` where entering
    /// `destination_key` from `direction` says it belongs.
    fn move_into_from_direction(
        &mut self,
        node_key: NodeKey,
        destination_key: NodeKey,
        direction: Direction,
    ) {
        let Some(destination_parent_key) = self.parent_of(destination_key) else {
            return;
        };

        if matches!(self.get_node(destination_key), Some(NodeData::Leaf(_))) {
            let Some(destination_idx) = self.child_index(destination_parent_key, destination_key)
            else {
                return;
            };

            if self.parent_of(node_key) == Some(destination_parent_key) {
                let Some(node_idx) = self.child_index(destination_parent_key, node_key) else {
                    return;
                };
                if let Some(parent) = self.get_container_mut(destination_parent_key) {
                    parent.children.swap(node_idx, destination_idx);
                    parent.child_percents.swap(node_idx, destination_idx);
                }
                return;
            }

            // A cousin's neighbour. The node arrives from the far side, so moving left lands
            // it to the cousin's right and moving right to its left.
            let insert_at = destination_idx + usize::from(direction.is_leading());
            self.reparent(node_key, destination_parent_key, insert_at);
            self.squash_workspace();
            return;
        }

        let Some(destination) = self.get_container(destination_key) else {
            return;
        };
        let destination_layout = destination.layout();
        let child_count = destination.child_count();

        if destination_layout.is_parallel_to(direction) {
            // Entering along the container's own axis, at the edge the move came from.
            let insert_at = if direction.is_leading() {
                child_count
            } else {
                0
            };
            self.reparent(node_key, destination_key, insert_at);
            self.squash_workspace();
            return;
        }

        // Entering across the container's axis says nothing about where inside it the node
        // belongs — any move into tabs, a horizontal one into a stack. So the same question
        // is asked again of whatever was last focused in there, however deep that goes.
        let Some(child_key) = destination.focused_child_key() else {
            return;
        };
        self.move_into_from_direction(node_key, child_key, direction);
    }

    /// Take a node out of its parent's child list and put it in `parent_key` at `insert_at`.
    fn reparent(&mut self, node_key: NodeKey, parent_key: NodeKey, insert_at: usize) {
        self.detach_child(node_key);
        if let Some(parent) = self.get_container_mut(parent_key) {
            parent.insert_child(insert_at, node_key);
        }
        self.set_parent(node_key, Some(parent_key));
    }

    /// Take a node out of its parent's child list, leaving it parentless.
    pub(super) fn detach_child(&mut self, node_key: NodeKey) -> Option<(NodeKey, usize)> {
        let parent_key = self.parent_of(node_key)?;
        let idx = self.child_index(parent_key, node_key)?;
        self.get_container_mut(parent_key)?.remove_child(idx);
        self.set_parent(node_key, None);
        Some((parent_key, idx))
    }

    pub(super) fn wrap_child_in_container(
        &mut self,
        parent_key: NodeKey,
        child_idx: usize,
        layout: Layout,
    ) -> Option<NodeKey> {
        let child_key = self.get_container(parent_key)?.child_key(child_idx)?;
        if matches!(self.get_node(child_key), Some(NodeData::Container(_))) {
            return Some(child_key);
        }

        let _ = self.get_container_mut(parent_key)?.remove_child(child_idx);
        self.set_parent(child_key, None);

        let mut wrapper = ContainerData::new(layout);
        wrapper.add_child(child_key);
        let wrapper_key = self.insert_node(NodeData::Container(wrapper));
        self.set_parent(child_key, Some(wrapper_key));

        self.get_container_mut(parent_key)?
            .insert_child(child_idx, wrapper_key);
        self.set_parent(wrapper_key, Some(parent_key));

        Some(wrapper_key)
    }
}
