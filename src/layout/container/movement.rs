//! Moving nodes in a direction: sibling swaps, container entry/escape.

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
    pub(in crate::layout) fn move_target_in_direction(
        &mut self,
        direction: Direction,
        target: TreeCommandTarget,
    ) -> bool {
        self.clear_focus_history();
        if self.root.is_none() {
            return false;
        }

        let Some((node_key, preserve_selected_container)) = self.resolve_move_source(target) else {
            return false;
        };

        let moved = self.perform_move(node_key, direction);
        if moved && preserve_selected_container {
            self.selected_key = Some(node_key);
        }
        moved
    }

    /// Resolve the command target into the node to actually move (escaping single-child
    /// wrapper containers) and whether container selection should be preserved.
    fn resolve_move_source(&mut self, target: TreeCommandTarget) -> Option<(NodeKey, bool)> {
        let (mut move_key, preserve_selected_container) = match target {
            TreeCommandTarget::Workspace => return None,
            TreeCommandTarget::Container(key) => {
                matches!(self.get_node(key), Some(NodeData::Container(_))).then_some((key, true))?
            }
            TreeCommandTarget::Leaf(key) => {
                matches!(self.get_node(key), Some(NodeData::Leaf(_))).then_some((key, false))?
            }
        };

        self.sync_container_focus_from_key(move_key);

        // Moving the only child of a non-material wrapper means moving the wrapper itself.
        // The root is never moved, so stop below it.
        while let Some(parent_key) = self.parent_of(move_key) {
            if self.parent_of(parent_key).is_none() {
                break;
            }
            let Some(parent) = self.get_container(parent_key) else {
                break;
            };
            if parent.child_count() == 1 && !parent.preserve_on_single() {
                move_key = parent_key;
                continue;
            }
            break;
        }

        (Some(move_key) != self.root).then_some((move_key, preserve_selected_container))
    }

    /// Try the movement strategies in order: step within a parallel parent, enter an
    /// adjacent container across a parallel ancestor, or escape towards the root.
    fn perform_move(&mut self, node_key: NodeKey, direction: Direction) -> bool {
        let Some(parent_key) = self.parent_of(node_key) else {
            return false;
        };
        let Some(parent_layout) = self.get_container(parent_key).map(|c| c.layout()) else {
            return false;
        };

        if parent_layout.is_parallel_to(direction) {
            return self.move_within_parallel_parent(
                node_key,
                parent_key,
                parent_layout,
                direction,
            );
        }

        if let Some(moved) = self.try_move_via_parallel_ancestor(node_key, direction) {
            return moved;
        }

        if self.parent_of(parent_key).is_none() {
            let Some(node_idx) = self.child_index(parent_key, node_key) else {
                return false;
            };
            return self.move_root_node_across_workspace(node_key, node_idx, direction);
        }

        self.move_node_to_grandparent(node_key, direction)
    }

    /// Move within a parent whose layout runs along `direction`: swap with the adjacent
    /// sibling, enter it if it is a container, or escape to the grandparent at the edge.
    fn move_within_parallel_parent(
        &mut self,
        node_key: NodeKey,
        parent_key: NodeKey,
        parent_layout: Layout,
        direction: Direction,
    ) -> bool {
        let Some(parent) = self.get_container(parent_key) else {
            return false;
        };
        let child_count = parent.child_count();
        let Some(node_idx) = self.child_index(parent_key, node_key) else {
            return false;
        };

        let Some(target_idx) = direction.sibling_index(node_idx, child_count) else {
            // At edge: escape to the grandparent, unless this parent is the root.
            if self.parent_of(parent_key).is_none() {
                return false;
            }
            return self.move_node_to_grandparent(node_key, direction);
        };

        let Some(target_key) = self
            .get_container(parent_key)
            .and_then(|c| c.child_key(target_idx))
        else {
            return false;
        };

        if matches!(parent_layout, Layout::SplitH | Layout::SplitV) {
            if let Some(target_container) = self.get_container(target_key) {
                let should_enter = target_container.layout() != parent_layout
                    || target_container.preserve_on_single();
                if should_enter {
                    let focus_idx = target_container.focused_child_index().unwrap_or(0);
                    return self
                        .move_node_into_container(node_key, target_key, direction, focus_idx);
                }
            }
        }

        if let Some(container) = self.get_container_mut(parent_key) {
            container.children.swap(node_idx, target_idx);
            container.child_percents.swap(node_idx, target_idx);
        }

        self.focus_node_key(node_key);
        true
    }

    /// Walk up until an ancestor's parent runs along `direction` and move into the adjacent
    /// container there. Returns None when no such ancestor target exists.
    fn try_move_via_parallel_ancestor(
        &mut self,
        node_key: NodeKey,
        direction: Direction,
    ) -> Option<bool> {
        let mut ancestor_key = self.parent_of(node_key)?;
        while let Some(ancestor_parent_key) = self.parent_of(ancestor_key) {
            let ancestor_parent_layout = self.get_container(ancestor_parent_key)?.layout();

            if !ancestor_parent_layout.is_parallel_to(direction) {
                ancestor_key = ancestor_parent_key;
                continue;
            }

            let ancestor_idx = self.child_index(ancestor_parent_key, ancestor_key)?;
            let ancestor_child_count = self.get_container(ancestor_parent_key)?.child_count();
            let target_idx = direction.sibling_index(ancestor_idx, ancestor_child_count)?;
            let target_key = self
                .get_container(ancestor_parent_key)?
                .child_key(target_idx)?;
            let focus_idx = self
                .get_container(target_key)?
                .focused_child_index()
                .unwrap_or(0);

            return Some(self.move_node_into_container(node_key, target_key, direction, focus_idx));
        }
        None
    }

    /// Move a node out of its parent and into its grandparent, next to that parent.
    pub(super) fn move_node_to_grandparent(
        &mut self,
        node_key: NodeKey,
        direction: Direction,
    ) -> bool {
        let Some(node_parent_key) = self.parent_of(node_key) else {
            return false;
        };
        let Some(grandparent_key) = self.parent_of(node_parent_key) else {
            return false;
        };
        let Some(node_idx) = self.child_index(node_parent_key, node_key) else {
            return false;
        };

        // Escaping into the workspace, across the orientation it lays its children out in,
        // does not stop at the workspace: it crosses it. Measured against sway 1.11 — the
        // workspace flips and everything else moves under one container.
        let escapes_across_the_workspace = Some(grandparent_key) == self.root
            && self
                .get_container(grandparent_key)
                .is_some_and(|root| !root.layout().is_parallel_to(direction));
        if escapes_across_the_workspace {
            if let Some(container) = self.get_container_mut(node_parent_key) {
                let _ = container.remove_child(node_idx);
            }
            self.set_parent(node_key, None);
            let Some(parent_idx) = self.child_index(grandparent_key, node_parent_key) else {
                return false;
            };
            if let Some(root) = self.get_container_mut(grandparent_key) {
                root.insert_child(parent_idx + 1, node_key);
            }
            self.set_parent(node_key, Some(grandparent_key));
            self.cleanup_containers(Some(node_parent_key));
            let Some(node_idx) = self.child_index(grandparent_key, node_key) else {
                return false;
            };
            return self.move_root_node_across_workspace(node_key, node_idx, direction);
        }
        let Some(parent_idx) = self.child_index(grandparent_key, node_parent_key) else {
            return false;
        };

        // An only child leaves its parent empty, so the parent is about to be collapsed and
        // the insertion index shifts accordingly.
        let parent_will_be_removed = self
            .get_container(node_parent_key)
            .is_some_and(|container| container.child_count() == 1);

        if let Some(container) = self.get_container_mut(node_parent_key) {
            let _ = container.remove_child(node_idx);
        } else {
            return false;
        }
        self.set_parent(node_key, None);

        let insert_at = if direction.is_leading() {
            if parent_will_be_removed {
                parent_idx.saturating_sub(1)
            } else {
                parent_idx
            }
        } else if parent_will_be_removed {
            parent_idx + 2
        } else {
            parent_idx + 1
        };

        if let Some(container) = self.get_container_mut(grandparent_key) {
            container.insert_child(insert_at, node_key);
        } else {
            return false;
        }
        self.set_parent(node_key, Some(grandparent_key));

        self.cleanup_containers(Some(node_parent_key));

        self.focus_node_key(node_key);

        true
    }

    /// Move a node into `target_key`, entering it from `direction`.
    pub(super) fn move_node_into_container(
        &mut self,
        node_key: NodeKey,
        target_key: NodeKey,
        direction: Direction,
        target_focus_idx: usize,
    ) -> bool {
        let Some(target) = self.get_container(target_key) else {
            return false;
        };
        let child_count = target.child_count();
        let target_layout = target.layout();

        // Entering along the container's own axis lands at the far edge; entering across it
        // lands beside the container's focused child.
        let insert_idx = match target_layout {
            Layout::SplitH | Layout::SplitV => {
                if target_layout.is_parallel_to(direction) {
                    if direction.is_leading() {
                        child_count
                    } else {
                        0
                    }
                } else if direction.is_leading() {
                    target_focus_idx + 1
                } else {
                    target_focus_idx
                }
            }
            Layout::Tabbed | Layout::Stacked => {
                if direction.is_leading() {
                    target_focus_idx
                } else {
                    target_focus_idx + 1
                }
            }
        };

        let Some(node_parent_key) = self.parent_of(node_key) else {
            return false;
        };
        let Some(node_idx) = self.child_index(node_parent_key, node_key) else {
            return false;
        };

        if let Some(container) = self.get_container_mut(node_parent_key) {
            let _ = container.remove_child(node_idx);
        } else {
            return false;
        }

        if let Some(container) = self.get_container_mut(target_key) {
            container.insert_child(insert_idx.min(child_count), node_key);
        } else {
            return false;
        }
        self.set_parent(node_key, Some(target_key));

        self.cleanup_containers(Some(node_parent_key));

        self.focus_node_key(node_key);

        true
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

    /// Move a top-level node across the workspace's own orientation.
    ///
    /// Measured against sway 1.11: the workspace flips to the direction's orientation, its
    /// other children move under a single container that keeps the old one, and the moved
    /// node becomes that container's sibling — before it going up or left, after it going
    /// down or right. Nothing is wrapped around the node being moved.
    ///
    /// Whether the new wrapper survives is left to the normal cleanup: holding one window
    /// under a workspace of the same orientation it already has, it dissolves, which is why
    /// moving back out again leaves the workspace flat.
    pub(super) fn move_root_node_across_workspace(
        &mut self,
        node_key: NodeKey,
        node_idx: usize,
        direction: Direction,
    ) -> bool {
        let Some(root_key) = self.root else {
            return false;
        };
        let Some(root) = self.get_container(root_key) else {
            return false;
        };
        if root.child_count() <= 1 {
            return false;
        }
        let previous = root.layout();

        let Some(root) = self.get_container_mut(root_key) else {
            return false;
        };
        if root.remove_child(node_idx).is_none() {
            return false;
        }

        if !self.wrap_workspace_children(previous, direction.split_layout()) {
            // Put it back rather than leave the tree short of a window.
            if let Some(root) = self.get_container_mut(root_key) {
                root.insert_child(node_idx, node_key);
            }
            return false;
        }

        let idx = usize::from(!direction.is_leading());
        let Some(root) = self.get_container_mut(root_key) else {
            return false;
        };
        root.insert_child(idx, node_key);
        self.set_parent(node_key, Some(root_key));
        self.sync_container_focus_from_key(node_key);

        // The wrapper may only re-state an orientation already expressed above it, in which
        // case it goes; cleanup decides, using the same rule it applies everywhere else.
        let wrapper_key = self
            .get_container(root_key)
            .and_then(|root| root.child_key(1 - idx));
        if let Some(wrapper_key) = wrapper_key {
            // Not preserved: the user did not ask for this container, so it only lives as
            // long as it expresses an orientation the workspace does not.
            if let Some(wrapper) = self.get_container_mut(wrapper_key) {
                wrapper.clear_preserve_on_single();
            }
            self.cleanup_containers(Some(wrapper_key));
        }
        true
    }
}
