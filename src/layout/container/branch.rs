//! One branch of the tree, addressed as a unit.
//!
//! The tree holds two kinds of branch now: the workspace's tiled side, and one per floating
//! group. Almost every operation that used to mean "the tree" meant "the tiled branch",
//! because there was nothing else it could mean. This is the vocabulary for saying which.
//!
//! sway needs no such type: its operations take a container and walk from it, so the branch
//! is wherever you started. Tiri's grew up around a single root and read it from `self`, so
//! the root has to be handed to them instead — which is what this does, and nothing more.

use smithay::utils::{Logical, Rectangle};

use super::{ContainerTree, Layout, LayoutElement, NodeKey};
use crate::layout::tile::Tile;

impl<W: LayoutElement> ContainerTree<W> {
    /// Lay out one branch in its own box, and apply the result.
    ///
    /// `arrange_floating` for one group. The tiled side goes through the ordinary pass, which
    /// already walks every branch; this is for a caller holding one group and nothing else.
    pub(in crate::layout) fn layout_branch(&mut self, branch_root: NodeKey) {
        let Some(area) = self.floating_area(branch_root) else {
            return;
        };
        let data = self.collect_branch_layout_data(branch_root, area);
        self.apply_layout_data(data);
    }

    /// How many windows a branch holds.
    pub(in crate::layout) fn window_count_in_branch(&self, branch_root: NodeKey) -> usize {
        self.leaf_keys_in_branch(branch_root).len()
    }

    /// The window a branch would hand back as focused — sway's `seat_get_focus_inactive_view`
    /// restricted to it.
    pub(in crate::layout) fn focused_window_in_branch(&self, branch_root: NodeKey) -> Option<&W> {
        let key = self.focus_inactive_view(branch_root)?;
        Some(self.get_tile(key)?.window())
    }

    /// The tiles of a branch, mutably.
    pub(in crate::layout) fn tiles_in_branch_mut(
        &mut self,
        branch_root: NodeKey,
    ) -> Vec<&mut Tile<W>> {
        let keys = self.leaf_keys_in_branch(branch_root);
        self.tiles_mut_for_keys(&keys)
            .into_iter()
            .map(|(_, tile)| tile)
            .collect()
    }

    /// The window ids of a branch, in tree order.
    pub(in crate::layout) fn window_ids_in_branch(&self, branch_root: NodeKey) -> Vec<W::Id> {
        self.leaf_keys_in_branch(branch_root)
            .into_iter()
            .filter_map(|key| Some(self.get_tile(key)?.window().id().clone()))
            .collect()
    }

    /// The layout of a branch's root container.
    pub(in crate::layout) fn branch_layout(&self, branch_root: NodeKey) -> Option<Layout> {
        self.get_container(branch_root).map(|c| c.layout())
    }

    /// How many children a branch's root has.
    pub(in crate::layout) fn branch_children_len(&self, branch_root: NodeKey) -> usize {
        self.get_container(branch_root)
            .map_or(0, |c| c.child_count())
    }

    /// The box a branch occupies, whichever side it is on.
    pub(in crate::layout) fn branch_area(&self, branch_root: NodeKey) -> Rectangle<f64, Logical> {
        self.floating_area(branch_root)
            .unwrap_or_else(|| self.layout_area())
    }
}
