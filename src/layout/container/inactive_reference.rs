//! Workspace-local inactive tiling focus, read directly from the seat order.

use super::{ContainerArena, InactiveTilingReference, LayoutElement, NodeData, NodeKey};

impl<W: LayoutElement> ContainerArena<W> {
    /// The most recent node that still belongs to `ws->tiling`.
    ///
    /// Tiling and floating share this arena and this seat order. Floating a subtree removes
    /// it from the tiled branch without disturbing the relative order of the tiled nodes,
    /// which is exactly the state sway reads through `seat_get_focus_inactive_tiling`.
    pub(in crate::layout) fn inactive_tiling_key(&self) -> Option<NodeKey> {
        self.seat.order().iter().copied().find(|key| {
            *key != self.root
                && self.get_node(*key).is_some()
                && self.branch_root(*key) == self.root
                && matches!(self.get_node(*key), Some(NodeData::Container(_)))
        })
    }

    /// Keep the exact tiling node the seat chose as the immediate unfloat context.
    pub(in crate::layout) fn inactive_tiling_reference(&self) -> Option<InactiveTilingReference> {
        Some(InactiveTilingReference::new(self.inactive_tiling_key()?))
    }

    /// Resolve an immediate unfloat reference without projecting it through a tree path.
    /// Containers receive the node as their last child; leaves receive it as their next sibling.
    pub(super) fn tiling_insertion_point(
        &self,
        reference: &InactiveTilingReference,
    ) -> Option<(NodeKey, usize)> {
        let key = reference.key();
        if self.get_node(key).is_none() || self.branch_root(key) != self.root {
            return None;
        }

        match self.get_node(key)? {
            NodeData::Workspace(_) => None,
            NodeData::Container(container) if !container.is_view() => {
                Some((key, container.child_count()))
            }
            _ => {
                let parent_key = self.parent_of(key)?;
                let parent = self.get_container(parent_key)?;
                let leaf_idx = self.child_index(parent_key, key)?;
                Some((parent_key, (leaf_idx + 1).min(parent.child_count())))
            }
        }
    }

    pub(in crate::layout) fn focus_inactive_tiling_key(&mut self, key: NodeKey) -> bool {
        if key == self.root || self.get_node(key).is_none() || self.branch_root(key) != self.root {
            return false;
        }
        self.focus_node_key(key);
        self.layout();
        true
    }

    pub(in crate::layout) fn window_for_inactive_tiling_key(&self, key: NodeKey) -> Option<&W> {
        if self.get_node(key).is_none() || self.branch_root(key) != self.root {
            return None;
        }
        self.get_tile(key).map(|tile| tile.window())
    }
}
