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

use super::ChildFractions;
use super::ContainerTree;
use super::Direction;
use super::LayoutElement;
use super::NodeData;
use super::NodeKey;
use super::TreeCommandTarget;

/// What a reparent does to the shares of the node it moves.
///
/// sway has seven reparenting sites in `cmd_move` and they do not agree. Six invalidate the
/// fraction of the container they just moved — the share was relative to a parent it has
/// left, so it means nothing where it lands. The seventh, promoting a node to sit beside an
/// ancestor, does the opposite: it keeps the moved node's and invalidates *the ancestor's*,
/// which never moved at all.
///
///     ancestor->pending.height = ancestor->pending.width = 0;
///     ancestor->height_fraction = ancestor->width_fraction = 0;   // move.c:408
///
/// i3 does what the six do. This follows sway, because the corpus is of sway, and
/// `promotion-invalidates-escaped-ancestor` is the recording that settles it.
#[derive(Clone, Copy)]
enum ReparentFractions {
    /// The moved node arrives with no share, to be filled in when the tree is arranged.
    Unset,
    /// The moved node keeps its share, and this ancestor loses its own instead.
    PreserveAndUnset(NodeKey),
}

impl<W: LayoutElement> ContainerTree<W> {
    /// Move the current command target in a direction.
    #[cfg(test)]
    pub(in crate::layout) fn move_in_direction(&mut self, direction: Direction) -> bool {
        let root = self.root;
        self.move_target_in_direction(direction, self.command_target_in(root))
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
            // Not `focus_node_key`: that raises the leaf's branch in every ancestor switcher
            // on the way to the root, which is the one thing `cmd_move` is careful not to do.
            self.set_seat_focus_preserving_switcher(leaf_key);
        }
        if preserve_selected_container && self.nodes.contains_key(node_key) {
            self.seat.keep_selected(node_key);
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

        // A branch's root has nowhere to go, and moving it is not one of these commands.
        self.parent_of(move_key)
            .is_some()
            .then_some((move_key, preserve_selected_container))
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
                if self.parent_of(parent_key).is_some() {
                    // Keep looking for a parallel parent.
                    current = parent_key;
                    continue;
                }
                // Nothing anywhere faces the right way, so the branch's root turns to face it.
                let Some(wrapper_key) =
                    self.wrap_branch_children(parent_key, parent_layout, direction.split_layout())
                else {
                    return false;
                };
                // `container->pending.height = container->pending.width = 0;` — sway
                // invalidates the command target the moment it reorients the workspace, and
                // the promotion below deliberately preserves it, so this earlier
                // invalidation is observable on its own.
                self.unset_node_fractions(node_key);
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

            if self.parent_of(parent_key).is_none() {
                // The node is at branch level with nothing beside it, which is where sway
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
        let node_parent_is_lone_top_level = self
            .parent_of(node_parent_key)
            .is_some_and(|grandparent| self.parent_of(grandparent).is_none())
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
        self.reparent(
            node_key,
            ancestor_parent_key,
            insert_at,
            ReparentFractions::PreserveAndUnset(ancestor_key),
        );

        let branch_root = self.branch_root(ancestor_parent_key);
        self.reap_empty(node_parent_key);
        self.squash_branch(branch_root);
        true
    }

    /// The shares a node holds in its current parent, whichever axis they were set on.
    fn node_fractions(&self, node_key: NodeKey) -> Option<ChildFractions> {
        let parent_key = self.parent_of(node_key)?;
        let idx = self.child_index(parent_key, node_key)?;
        Some(self.get_container(parent_key)?.child_fractions(idx))
    }

    /// Wipe a node's shares where it stands — sway zeroing `width_fraction`/`height_fraction`
    /// on a container it is not moving.
    fn unset_node_fractions(&mut self, node_key: NodeKey) {
        let Some(parent_key) = self.parent_of(node_key) else {
            return;
        };
        let Some(idx) = self.child_index(parent_key, node_key) else {
            return;
        };
        if let Some(parent) = self.get_container_mut(parent_key) {
            parent.unset_child_fractions(idx);
        }
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
                    parent.swap_child_slots(node_idx, destination_idx);
                }
                return;
            }

            // A cousin's neighbour. The node arrives from the far side, so moving left lands
            // it to the cousin's right and moving right to its left.
            let insert_at = destination_idx + usize::from(direction.is_leading());
            let branch_root = self.branch_root(destination_parent_key);
            self.reparent(
                node_key,
                destination_parent_key,
                insert_at,
                ReparentFractions::Unset,
            );
            self.squash_branch(branch_root);
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
            let branch_root = self.branch_root(destination_key);
            self.reparent(
                node_key,
                destination_key,
                insert_at,
                ReparentFractions::Unset,
            );
            self.squash_branch(branch_root);
            return;
        }

        // Entering across the container's axis says nothing about where inside it the node
        // belongs — any move into tabs, a horizontal one into a stack. So the same question
        // is asked again of whatever was last focused in there, however deep that goes.
        let Some(child_key) = self.active_child(destination_key) else {
            return;
        };
        self.move_into_from_direction(node_key, child_key, direction);
    }

    /// Take a node out of its parent's child list and put it in `parent_key` at `insert_at`.
    ///
    /// Every one of sway's reparenting sites in `cmd_move` follows the insert with
    /// `width_fraction = height_fraction = 0` on the container it just moved, so that is here
    /// rather than at each caller. The share it had was relative to a parent it has left, and
    /// what it should be here is not decided until the command ends and the whole list is
    /// resolved.
    ///
    /// sway has one exception, and it is not reproduced: promoting a node to sit beside an
    /// ancestor invalidates *the ancestor's* fraction and keeps the moved node's, which is
    /// the two the wrong way round — i3 does what this does, and
    /// `nested-same-orientation-after-a-move` is the recording of the difference.
    fn reparent(
        &mut self,
        node_key: NodeKey,
        parent_key: NodeKey,
        insert_at: usize,
        fractions: ReparentFractions,
    ) {
        let carried = match fractions {
            ReparentFractions::Unset => None,
            ReparentFractions::PreserveAndUnset(_) => self.node_fractions(node_key),
        };
        self.detach_child(node_key);
        if let Some(parent) = self.get_container_mut(parent_key) {
            // `insert_child_unset`, never `insert_child`: sway's `container_insert_child`
            // puts the node in the list and leaves every sibling's fraction exactly as it
            // was. Redistributing here rewrites raw values the end-of-command resolve is
            // about to read, and the answer comes out of the wrong numbers.
            parent.insert_child_unset(insert_at, node_key);
            let idx = parent
                .children()
                .iter()
                .position(|key| *key == node_key)
                .unwrap_or(insert_at);
            // The moved node becomes the most recently focused of its new siblings, and of
            // nobody else's. sway has no per-container order to update — it has one focus
            // stack per seat, and `seat_get_active_tiling_child` reads which tab a switcher
            // shows off it: the first entry whose *direct parent* is that switcher.
            // `cmd_move` never touches that stack. What changes the answer is that the moved
            // node's parent changed, so it now answers for its new parent and has stopped
            // answering for its old one.
            //
            // Which is why the switcher it left goes back to showing something else without
            // being told, and why the switchers *above* the destination do not move at all:
            // the node was never their direct child and still is not.
            match carried {
                Some(carried) => parent.set_child_fractions(idx, carried),
                None => parent.unset_child_fractions(idx),
            }
        }
        self.set_parent(node_key, Some(parent_key));
        if let ReparentFractions::PreserveAndUnset(ancestor_key) = fractions {
            self.unset_node_fractions(ancestor_key);
        }
    }

    /// Take a node out of its parent's child list, leaving it parentless.
    pub(super) fn detach_child(&mut self, node_key: NodeKey) -> Option<(NodeKey, usize)> {
        let parent_key = self.parent_of(node_key)?;
        let idx = self.child_index(parent_key, node_key)?;
        self.get_container_mut(parent_key)?.remove_child(idx);
        self.set_parent(node_key, None);
        Some((parent_key, idx))
    }
}
