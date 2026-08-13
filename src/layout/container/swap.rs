//! Two nodes trade places.
//!
//! sway's `container_swap` (`sway/tree/container.c:1863`), reached from `swap container with
//! id|con_id|mark <arg>`. It is not a move: nothing climbs, nothing is reaped, no parent is
//! squashed afterwards. The two nodes exchange slots and the arrange that follows finds each
//! of them where the other was.
//!
//! What travels is the slot's size, not the node's. `swap_places` copies each container's
//! `width_fraction`/`height_fraction` onto the other before reinserting them, so a small
//! window swapped into a large slot comes out large. That is the opposite of the
//! neighbour-swap inside `cmd_move`, where the fractions stay with the nodes — see
//! [`LayoutParentData::swap_child_slots`]. Two commands, two rules, and the difference is
//! observable the moment the two slots are not the same size.

use super::ContainerTree;
use super::LayoutElement;
use super::NodeKey;

impl<W: LayoutElement> ContainerTree<W> {
    /// Exchange the places of two nodes, and their shares with them.
    ///
    /// Returns whether anything moved. The refusals are sway's, plus one of tiri's own:
    ///
    /// - a node with itself, and a node with its own ancestor or descendant — sway rejects
    ///   both in `cmd_swap` before it calls this;
    /// - the workspace root, which has no slot to trade;
    /// - two nodes in different branches of the workspace. sway swaps across `ws->tiling`
    ///   and `ws->floating` freely because a floating con is a con like any other, while
    ///   here the floating root carries the geometry that makes it float, so moving one
    ///   under a tiling parent would leave a node claiming to be both. Crossing the two
    ///   sides is what `float_subtree`/`unfloat_subtree` are for, and a swap that needs
    ///   them is not this function.
    pub(in crate::layout) fn swap_nodes(&mut self, a: NodeKey, b: NodeKey) -> bool {
        if a == b || self.get_node(a).is_none() || self.get_node(b).is_none() {
            return false;
        }
        if self.is_descendant(a, b) || self.is_descendant(b, a) {
            return false;
        }
        if self.branch_root(a) != self.branch_root(b) {
            return false;
        }

        let (Some(parent_a), Some(parent_b)) = (self.parent_of(a), self.parent_of(b)) else {
            return false;
        };
        let (Some(idx_a), Some(idx_b)) =
            (self.child_index(parent_a, a), self.child_index(parent_b, b))
        else {
            return false;
        };

        let fractions_a = self.node_fractions(a);
        let fractions_b = self.node_fractions(b);
        if let Some(fractions) = fractions_b {
            self.set_node_fractions(a, fractions);
        }
        if let Some(fractions) = fractions_a {
            self.set_node_fractions(b, fractions);
        }

        if parent_a == parent_b {
            if let Some(parent) = self.get_container_mut(parent_a) {
                parent.swap_child_slots(idx_a, idx_b);
            }
        } else {
            // Removing from one parent cannot shift an index in the other, so each slot is
            // filled where it was.
            if let Some(parent) = self.get_container_mut(parent_a) {
                parent.remove_child(idx_a);
                parent.insert_child(idx_a, b);
            }
            if let Some(parent) = self.get_container_mut(parent_b) {
                parent.remove_child(idx_b);
                parent.insert_child(idx_b, a);
            }
            self.set_parent(a, Some(parent_b));
            self.set_parent(b, Some(parent_a));
        }

        true
    }

    /// Swap what the seat has selected with the window `target`.
    ///
    /// The selection, not the focused leaf: `cmd_swap` operates on
    /// `config->handler_context.container`, which after `focus parent` is the container. A
    /// swap therefore trades whole subtrees when one is selected, and sway's own refusals
    /// cover the case where that subtree contains the target.
    pub(in crate::layout) fn swap_selected_with_window(&mut self, target: &W::Id) -> bool {
        let Some(selected) = self.selected_node_key() else {
            return false;
        };
        let Some(target) = self.window_key(target) else {
            return false;
        };
        self.swap_nodes(selected, target)
    }
}
