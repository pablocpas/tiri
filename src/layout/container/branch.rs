//! One branch of the tree, addressed as a unit.
//!
//! The tree holds two kinds of branch now: the workspace's tiled side, and one per floating
//! group. Almost every operation that used to mean "the tree" meant "the tiled branch",
//! because there was nothing else it could mean. This is the vocabulary for saying which.
//!
//! sway needs no such type: its operations take a container and walk from it, so the branch
//! is wherever you started. Tiri's grew up around a single root and read it from `self`, so
//! the root has to be handed to them instead — which is what this does, and nothing more.

#[cfg(test)]
use super::Layout;
use super::{ContainerArena, ContainerData, LayoutElement, NodeKey, TabBarInfo};
use crate::layout::tile::Tile;

impl<W: LayoutElement> ContainerArena<W> {
    /// The workspace's own root — the tiled branch.
    pub(in crate::layout) fn workspace_root(&self) -> NodeKey {
        self.root
    }

    /// Whether a branch holds no windows.
    pub(in crate::layout) fn branch_is_empty(&self, branch_root: NodeKey) -> bool {
        self.first_leaf_in_branch(branch_root).is_none()
    }

    /// The container a branch's root is, when it is one.
    pub(in crate::layout) fn branch_container(
        &self,
        branch_root: NodeKey,
    ) -> Option<&ContainerData<W>> {
        self.get_real_container(branch_root)
    }

    /// The tab bars inside one branch, addressed from its own root.
    pub(in crate::layout) fn tab_bar_layouts_in_branch(
        &self,
        branch_root: NodeKey,
    ) -> Vec<TabBarInfo> {
        let mut out = Vec::new();
        let mut path = Vec::new();
        self.collect_tab_bar_layouts(branch_root, &mut path, &mut out, true);
        out
    }

    /// Lay out after changing one branch.
    ///
    /// The arena has one transaction and one committed geometry cache, so a branch cannot be
    /// applied independently without bypassing size requests and overwriting an in-flight
    /// transaction. `arrange_workspace` still visits `arrange_children` and every
    /// `arrange_floating` branch separately inside the pass; unchanged leaves do not receive a
    /// request. The branch argument makes the caller's ownership explicit and rejects a stale
    /// root, while the shared pass keeps the commit atomic.
    pub(in crate::layout) fn layout_branch(&mut self, branch_root: NodeKey) {
        if self.floating_area(branch_root).is_none() {
            return;
        }
        self.layout();
    }

    /// How many windows a branch holds.
    pub(in crate::layout) fn window_count_in_branch(&self, branch_root: NodeKey) -> usize {
        self.leaf_keys_in_branch(branch_root).len()
    }

    /// The window a branch would hand back as focused — sway's `seat_get_focus_inactive_view`
    /// restricted to it.
    pub(in crate::layout) fn focused_window_in_branch(&self, branch_root: NodeKey) -> Option<&W> {
        let key = self.focus_inactive_view_in_branch(branch_root)?;
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

    /// The layout of a branch's root container.
    #[cfg(test)]
    pub(in crate::layout) fn branch_layout(&self, branch_root: NodeKey) -> Option<Layout> {
        self.get_container(branch_root).map(|c| c.layout())
    }

    /// How many children a branch's root has.
    pub(in crate::layout) fn branch_children_len(&self, branch_root: NodeKey) -> usize {
        self.get_container(branch_root)
            .map_or(0, |c| c.child_count())
    }
}
