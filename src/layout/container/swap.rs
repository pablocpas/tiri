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

use super::ContainerArena;
use super::FloatingRootKind;
use super::Layout;
use super::LayoutElement;
use super::NodeKey;

impl<W: LayoutElement> ContainerArena<W> {
    fn focus_swap_node(&mut self, key: NodeKey) {
        if self.get_node(key).is_some_and(|node| node.is_view()) {
            self.focus_node_key(key);
        } else {
            self.select_container(key);
        }
    }

    /// Rebuild the seat ancestry after two nodes have changed parents.
    ///
    /// sway's `swap_focus` always focuses the same node again. When that node arrived from a
    /// tabbed/stacked parent, it first focuses the other swapped node so both switchers'
    /// active-child entries are updated in the same order as sway's global focus stack.
    fn restore_swap_focus(&mut self, a: NodeKey, b: NodeKey, focused: Option<NodeKey>) {
        let Some(focused) = focused.filter(|key| self.holds_node(*key)) else {
            return;
        };
        let parent_is_switcher = |this: &Self, key| {
            this.parent_of(key)
                .and_then(|parent| this.get_container(parent))
                .is_some_and(|parent| matches!(parent.layout(), Layout::Tabbed | Layout::Stacked))
        };

        if focused == a && parent_is_switcher(self, b) {
            self.focus_swap_node(b);
        } else if focused == b && parent_is_switcher(self, a) {
            self.focus_swap_node(a);
        }

        // `seat_set_focus` returns early when the node it is given is already the focused
        // one, so the ordinary swap — the one that did not just focus the other node —
        // reaches the seat not at all. Nothing is raised, and every container goes on
        // listing its children in the order the swap found them: what a switcher shows is
        // decided by where the nodes *were*, so the branch that held the focused window is
        // still the one on top, now holding whatever the swap put into it.
        if self.seat.node() != Some(focused) {
            self.focus_swap_node(focused);
        }
    }

    fn swap_node_fractions(&mut self, a: NodeKey, b: NodeKey) {
        let fractions_a = self.node_fractions(a);
        let fractions_b = self.node_fractions(b);
        if let Some(fractions) = fractions_b {
            self.set_node_fractions(a, fractions);
        }
        if let Some(fractions) = fractions_a {
            self.set_node_fractions(b, fractions);
        }
    }

    fn swap_node_geometries(&mut self, a: NodeKey, b: NodeKey) {
        let geometry_a = self.node_geometry(a);
        let geometry_b = self.node_geometry(b);
        if let Some(geometry) = geometry_b {
            self.set_node_geometry(a, geometry);
        }
        if let Some(geometry) = geometry_a {
            self.set_node_geometry(b, geometry);
        }
    }

    fn swap_fullscreen_slot(&mut self, a: NodeKey, b: NodeKey) {
        let replacement = if self.fullscreen_key() == Some(a) {
            self.transfer_fullscreen_to_replacement(a, b);
            Some(b)
        } else if self.fullscreen_key() == Some(b) {
            self.transfer_fullscreen_to_replacement(b, a);
            Some(a)
        } else {
            None
        };

        let Some(replacement) = replacement else {
            return;
        };

        // `container_swap` disables fullscreen before exchanging the slots and enables it on
        // the replacement afterwards. Enabling workspace fullscreen calls
        // `seat_set_focus_container` on its new owner. Descendant-only swaps never enter this
        // path because the exact fullscreen node did not change.
        if self
            .get_node(replacement)
            .is_some_and(|node| node.is_view())
        {
            self.focus_node_key(replacement);
        } else {
            self.select_container(replacement);
        }
    }

    /// Exchange the places of two nodes, and their shares with them.
    ///
    /// Returns whether anything moved. The refusals are sway's, plus one of tiri's own:
    ///
    /// - a node with itself, and a node with its own ancestor or descendant — sway rejects
    ///   both in `cmd_swap` before it calls this;
    /// - the workspace root, which has no slot to trade;
    ///
    /// A child below a floating wrapper can swap with a tiled child: the two parent slots
    /// remain where they are, so the wrapper keeps owning the floating geometry and the
    /// arriving subtree simply occupies its place. A floating root itself has no parent and
    /// is rejected by the ordinary root rule below; swapping roots requires coordinating the
    /// floating stack, not pretending the root has a parent slot.
    pub(in crate::layout) fn swap_nodes(&mut self, a: NodeKey, b: NodeKey) -> bool {
        if a == b || self.get_node(a).is_none() || self.get_node(b).is_none() {
            return false;
        }
        if self.is_descendant(a, b) || self.is_descendant(b, a) {
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
        let focused = self.seat.node();

        // `swap_places` exchanges the complete pending slots before either parent is arranged.
        // Geometry matters independently of fractions while workspace fullscreen prevents one
        // of those parents from being visited.
        self.swap_node_geometries(a, b);
        self.swap_node_fractions(a, b);

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

        self.restore_swap_focus(a, b, focused);
        self.swap_fullscreen_slot(a, b);

        true
    }

    /// Swap nodes when at least one of their slots is a top-level floating slot.
    ///
    /// Floating roots have the workspace as a semantic parent but are deliberately absent
    /// from its tiled child list. The ordinary parent/index exchange therefore cannot express
    /// their slots. The authoritative floating stack lives in this arena, so topology, root
    /// identity and slot geometry change atomically.
    pub(in crate::layout) fn swap_nodes_at_floating_boundary(
        &mut self,
        a: NodeKey,
        b: NodeKey,
    ) -> bool {
        if a == b || self.get_node(a).is_none() || self.get_node(b).is_none() {
            return false;
        }
        if self.is_descendant(a, b) || self.is_descendant(b, a) {
            return false;
        }
        let focused = self.seat.node();

        let a_root = self.is_floating_root(a);
        let b_root = self.is_floating_root(b);
        match (a_root, b_root) {
            (false, false) => false,
            (true, true) => {
                // Floating-list slots do not appear in the workspace child array, but their
                // nodes still exchange the same pending x/y/width/height as any other
                // `swap_places` pair. `FloatingGeometry` below transfers the target slot;
                // this exchanges the independently observable pending boxes.
                self.swap_node_geometries(a, b);
                let Some(a_idx) = self.floating_root_index(a) else {
                    return false;
                };
                let Some(b_idx) = self.floating_root_index(b) else {
                    return false;
                };
                let (a_id, a_kind) = (
                    self.floating_roots[a_idx].id,
                    self.floating_roots[a_idx].kind,
                );
                self.floating_roots[a_idx].key = b;
                self.floating_roots[a_idx].id = self.floating_roots[b_idx].id;
                self.floating_roots[a_idx].kind = self.floating_roots[b_idx].kind;
                self.floating_roots[b_idx].key = a;
                self.floating_roots[b_idx].id = a_id;
                self.floating_roots[b_idx].kind = a_kind;
                self.swap_node_fractions(a, b);
                self.restore_swap_focus(a, b, focused);
                self.swap_fullscreen_slot(a, b);
                true
            }
            _ => {
                let (root, node) = if a_root { (a, b) } else { (b, a) };
                let Some(parent) = self.parent_of(node) else {
                    return false;
                };
                let Some(idx) = self.child_index(parent, node) else {
                    return false;
                };
                let Some(root_idx) = self.floating_root_index(root) else {
                    return false;
                };

                self.swap_node_geometries(root, node);
                self.swap_node_fractions(root, node);
                let Some(parent_data) = self.get_container_mut(parent) else {
                    return false;
                };
                parent_data.children[idx] = root;
                self.set_parent(root, Some(parent));
                self.set_parent(node, Some(self.root));
                self.floating_roots[root_idx].key = node;
                self.floating_roots[root_idx].kind = if self.is_leaf(node) {
                    FloatingRootKind::ImplicitWindowGroup
                } else {
                    FloatingRootKind::FloatedContainer
                };
                self.restore_swap_focus(root, node, focused);
                self.swap_fullscreen_slot(root, node);

                // The exchanged leaves changed branch ownership. A fullscreen arrange below
                // can run immediately afterwards; if cached addresses still name the old
                // branches, that partial pass treats the newly floating leaf as stale tiled
                // data and drops the pending box just exchanged above.
                self.readdress_leaf_layouts();

                true
            }
        }
    }
}
