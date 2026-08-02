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

        // What moves is what was asked for. Measured: a window that is the only child of a
        // container comes out of it, and the container it leaves is the cleanup's business
        // rather than travelling along with it.

        // A window that is the whole workspace can still be moved: nothing shifts, but the
        // workspace takes the direction's orientation, which is what sway does. A root
        // *container* cannot — moving the workspace itself is not one of these commands.
        let root_container = Some(move_key) == self.root
            && !matches!(self.get_node(move_key), Some(NodeData::Leaf(_)));
        (!root_container).then_some((move_key, preserve_selected_container))
    }

    /// Try the movement strategies in order: step within a parallel parent, enter an
    /// adjacent container across a parallel ancestor, or escape towards the root.
    fn perform_move(&mut self, node_key: NodeKey, direction: Direction) -> bool {
        // The whole workspace is this one node, so there is nothing to move past — but the
        // workspace can still take the direction's orientation, which is what sway does.
        let Some(parent_key) = self.parent_of(node_key) else {
            return self.move_root_node_across_workspace(node_key, 0, direction);
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
            // At the edge, so the move leaves this container. Where it lands is the same
            // question the perpendicular case asks: climb until something faces the right
            // way and has room, rather than stopping at the first container up. Dropping
            // into a grandparent laid out across the direction places the window sideways,
            // which is not the move that was asked for.
            if self.parent_of(parent_key).is_none() {
                return false;
            }
            if let Some(moved) = self.try_move_via_parallel_ancestor(node_key, direction) {
                return moved;
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
                    || target_container.is_user_container();
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
        // The outermost ancestor laid out along the direction, remembered while climbing:
        // if nothing anywhere has room, that is where the move ends up, at its far edge.
        let mut outermost_parallel = None;
        while let Some(ancestor_parent_key) = self.parent_of(ancestor_key) {
            let ancestor_parent_layout = self.get_container(ancestor_parent_key)?.layout();

            if !ancestor_parent_layout.is_parallel_to(direction) {
                ancestor_key = ancestor_parent_key;
                continue;
            }

            outermost_parallel = Some((ancestor_parent_key, ancestor_key));
            let ancestor_idx = self.child_index(ancestor_parent_key, ancestor_key)?;
            let ancestor_child_count = self.get_container(ancestor_parent_key)?.child_count();
            // No room at this level: keep climbing rather than give up. Measured against
            // sway 1.11 — a move bubbles until it can happen, and only stops for good at
            // the workspace, which the caller handles by crossing it.
            let Some(target_idx) = direction.sibling_index(ancestor_idx, ancestor_child_count)
            else {
                ancestor_key = ancestor_parent_key;
                continue;
            };
            let target_key = self
                .get_container(ancestor_parent_key)?
                .child_key(target_idx)?;
            let focus_idx = self
                .get_container(target_key)?
                .focused_child_index()
                .unwrap_or(0);

            return Some(self.move_node_into_container(node_key, target_key, direction, focus_idx));
        }

        // Nothing above had room, so the move ends beside the outermost thing that faces the
        // right way — measured against sway 1.11, where a move keeps climbing until it can
        // happen rather than stopping at the first ancestor that cannot take it.
        let (container_key, through_key) = outermost_parallel?;

        // Nothing to leave behind: if the node is the only thing on the way up to where it
        // would land, it is already there and the move rearranges nothing. sway does nothing
        // in that case, and a move that changes no arrangement must not dissolve the
        // containers it passes through either.
        let mut key = node_key;
        let mut alone = true;
        while key != through_key {
            let Some(parent_key) = self.parent_of(key) else {
                break;
            };
            if self
                .get_container(parent_key)
                .is_some_and(|p| p.child_count() > 1)
            {
                alone = false;
                break;
            }
            key = parent_key;
        }
        if alone {
            return None;
        }

        let through_idx = self.child_index(container_key, through_key)?;
        let insert_at = if direction.is_leading() {
            through_idx
        } else {
            through_idx + 1
        };
        Some(self.move_node_to(node_key, container_key, insert_at))
    }

    /// Take `node_key` out of wherever it is and put it in `container_key` at `insert_at`.
    fn move_node_to(
        &mut self,
        node_key: NodeKey,
        container_key: NodeKey,
        insert_at: usize,
    ) -> bool {
        let Some(parent_key) = self.parent_of(node_key) else {
            return false;
        };
        let Some(idx) = self.child_index(parent_key, node_key) else {
            return false;
        };
        if self
            .get_container_mut(parent_key)
            .and_then(|parent| parent.remove_child(idx))
            .is_none()
        {
            return false;
        }
        let Some(container) = self.get_container_mut(container_key) else {
            return false;
        };
        container.insert_child(insert_at, node_key);
        self.set_parent(node_key, Some(container_key));

        let leaf_key = self.leaf_under_key(node_key);
        self.cleanup_containers(Some(parent_key));
        if let Some(leaf_key) = leaf_key {
            self.focus_node_key(leaf_key);
        }
        true
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
            // Re-read where the node ended up rather than trusting `grandparent_key`: the
            // cleanup can dissolve the container it left *and* collapse the root onto the
            // node itself, and asking the old root for an index then answers nothing — which
            // used to skip the crossing below and leave the workspace facing the way it was.
            let node_idx = self
                .root
                .and_then(|root_key| self.child_index(root_key, node_key))
                .unwrap_or(0);
            // The escape above already restructured the tree, so this reports a change
            // whatever the crossing decides — a mutation that reports none skips the
            // relayout that keeps the cached geometry addressed to the right nodes.
            self.move_root_node_across_workspace(node_key, node_idx, direction);
            return true;
        }
        let Some(parent_idx) = self.child_index(grandparent_key, node_parent_key) else {
            return false;
        };

        // Nothing to move past: the node is all its parent holds and the parent is all the
        // grandparent holds, so leaving would put it exactly where it already is. sway does
        // nothing here, and so must this — a move that rearranges nothing must not dissolve
        // a container on its way out either.
        let alone_all_the_way_up = self
            .get_container(node_parent_key)
            .is_some_and(|parent| parent.child_count() == 1)
            && self
                .get_container(grandparent_key)
                .is_some_and(|grandparent| grandparent.child_count() == 1);
        if alone_all_the_way_up {
            return false;
        }

        if let Some(container) = self.get_container_mut(node_parent_key) {
            let _ = container.remove_child(node_idx);
        } else {
            return false;
        }
        self.set_parent(node_key, None);

        // Beside the container it came out of, on the side the move points to. Whether that
        // container survives is cleanup's business: removing it later does not move what was
        // placed relative to it, and anticipating the removal here is what used to carry a
        // window one position too far.
        let insert_at = if direction.is_leading() {
            parent_idx
        } else {
            parent_idx + 1
        };

        if let Some(container) = self.get_container_mut(grandparent_key) {
            container.insert_child(insert_at, node_key);
        } else {
            return false;
        }
        self.set_parent(node_key, Some(grandparent_key));

        // Cleanup may dissolve the very node that moved — a lone wrapper carried up with its
        // window is exactly the shape it removes — so remember a leaf to focus first.
        let leaf_key = self.leaf_under_key(node_key);
        self.cleanup_containers(Some(node_parent_key));
        if let Some(leaf_key) = leaf_key {
            self.focus_node_key(leaf_key);
        }

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
        // Entering across a container's axis means landing beside its focused child — and
        // when that child is itself a container, the move carries on into it. Measured
        // against sway 1.11: a window that left a nested split comes back to the same place
        // rather than stopping beside it.
        let mut target_key = target_key;
        let mut target_focus_idx = target_focus_idx;
        while let Some(target) = self.get_container(target_key) {
            if target.layout().is_parallel_to(direction) {
                break;
            }
            let Some(child_key) = target.child_key(target_focus_idx) else {
                break;
            };
            // Never descend into what is being moved: a node cannot land inside itself.
            if child_key == node_key || self.is_descendant(child_key, node_key) {
                break;
            }
            let Some(child) = self.get_container(child_key) else {
                break;
            };
            target_focus_idx = child.focused_child_index().unwrap_or(0);
            target_key = child_key;
        }

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
            // Tabs have no axis to enter along, so the direction says nothing about where
            // in the stack the newcomer belongs. Measured: it takes the focused child's
            // place and pushes it back, whichever side it arrived from.
            Layout::Tabbed | Layout::Stacked => target_focus_idx,
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

        // As above: what moved may not survive cleanup, but the window inside it does.
        let leaf_key = self.leaf_under_key(node_key);
        self.cleanup_containers(Some(node_parent_key));
        if let Some(leaf_key) = leaf_key {
            self.focus_node_key(leaf_key);
        }

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
        let previous = self.root_container_layout();

        // With nothing to move past, there is still an orientation to take: measured, sway
        // turns the workspace to face the direction and leaves the window where it is. The
        // wrap below would have nothing to wrap. A workspace whose only child is a window
        // has no root container at all, and lands here too.
        let alone = self
            .get_container(root_key)
            .is_none_or(|root| root.child_count() <= 1);
        if alone {
            let layout = direction.split_layout();
            if layout == previous {
                return false;
            }
            self.set_workspace_layout_hint(layout);
            self.set_root_container_layout(layout);
            return true;
        }

        let Some(root) = self.get_container_mut(root_key) else {
            return false;
        };
        if root.remove_child(node_idx).is_none() {
            return false;
        }

        // What stays behind is always wrapped in a container keeping the old orientation.
        // Whether that wrapper survives is not decided here: a wrapper holding one split
        // that says what the workspace now says is redundant, and the cleanup below removes
        // it by splicing — which is how the same rule produces a doubled container in one
        // measured case and a flat workspace in another.
        if self
            .wrap_workspace_children(previous, direction.split_layout())
            .is_none()
        {
            // Put it back rather than leave the tree short of a window.
            if let Some(root) = self.get_container_mut(root_key) {
                root.insert_child(node_idx, node_key);
            }
            return false;
        }

        // Before everything that stayed, or after all of it: a splice turns one child into
        // several, so the far end is not a fixed index.
        let Some(root) = self.get_container_mut(root_key) else {
            return false;
        };
        let idx = if direction.is_leading() {
            0
        } else {
            root.child_count()
        };
        root.insert_child(idx, node_key);
        self.set_parent(node_key, Some(root_key));
        self.sync_container_focus_from_key(node_key);

        // The wrapper may only be repeating what the workspace now says, in which case the
        // normalization removes it — the same pass, and the same rule, that runs after every
        // other move.
        let wrapper_key = self
            .get_container(root_key)
            .and_then(|root| root.child_key(if direction.is_leading() { 1 } else { 0 }));
        if let Some(wrapper_key) = wrapper_key {
            self.cleanup_containers(Some(wrapper_key));
        }
        true
    }
}
