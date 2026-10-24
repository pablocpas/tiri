//! Test-only, workspace-bound views of the shared container tree.

use smithay::utils::{Logical, Rectangle, Size};

use super::{ContainerTree, Layout, LayoutElement, NodeKey, Tile, Workspace};

impl<W: LayoutElement> Workspace<W> {
    pub fn has_windows(&self, containers: &ContainerTree<W>) -> bool {
        containers.workspace_has_windows(self.workspace_key())
    }

    pub fn is_floating(&self, containers: &ContainerTree<W>, window: &W::Id) -> bool {
        containers.window_is_floating_in(self.workspace_key(), window)
    }

    pub fn view_size(&self, containers: &ContainerTree<W>) -> Size<f64, Logical> {
        containers.view_size(self.workspace_key())
    }

    pub fn working_area(&self, containers: &ContainerTree<W>) -> Rectangle<f64, Logical> {
        containers.working_area(self.workspace_key())
    }

    pub fn windows<'a>(
        &self,
        containers: &'a ContainerTree<W>,
    ) -> impl Iterator<Item = &'a W> + 'a {
        containers.windows_in_workspace(self.workspace_key())
    }

    pub fn tiles<'a>(
        &self,
        containers: &'a ContainerTree<W>,
    ) -> impl Iterator<Item = &'a Tile<W>> + 'a {
        containers.tiles_in_workspace(self.workspace_key())
    }

    pub fn tiles_mut<'a>(
        &self,
        containers: &'a mut ContainerTree<W>,
    ) -> impl Iterator<Item = &'a mut Tile<W>> + 'a {
        containers
            .tiles_mut_in_workspace(self.workspace_key())
            .into_iter()
    }

    pub fn tiles_with_render_positions_mut<'a>(
        &self,
        containers: &'a mut ContainerTree<W>,
        round: bool,
    ) -> impl Iterator<Item = (&'a mut Tile<W>, smithay::utils::Point<f64, Logical>)> + 'a {
        containers.workspace_tiles_with_render_positions_mut(self.workspace_key(), round)
    }
}

/// One workspace's view of the layout-wide tree.
///
/// Binding the workspace key keeps assertions local without inventing another owner for nodes.
pub struct WorkspaceTreeView<'a, W: LayoutElement> {
    pub(super) containers: &'a ContainerTree<W>,
    pub(super) workspace: NodeKey,
}

impl<'a, W: LayoutElement> WorkspaceTreeView<'a, W> {
    pub(in crate::layout) fn tree(&self) -> &'a ContainerTree<W> {
        self.containers
    }

    pub fn workspace(&self) -> NodeKey {
        self.workspace
    }

    pub fn debug_tree(&self) -> String
    where
        W::Id: std::fmt::Display,
    {
        self.containers.debug_tree(self.workspace)
    }

    pub fn tiles(&self) -> impl Iterator<Item = &'a Tile<W>> + 'a {
        self.containers.tiled_tiles(self.workspace)
    }

    pub fn windows(&self) -> impl Iterator<Item = &'a W> + 'a {
        self.containers
            .tiled_tiles(self.workspace)
            .map(Tile::window)
    }

    pub fn focused_window_id(&self) -> Option<W::Id> {
        self.containers.focused_window_id(self.workspace)
    }

    pub fn selected_is_container(&self) -> bool {
        self.containers.selected_is_container(self.workspace)
    }

    pub fn has_containers(&self) -> bool {
        self.containers.has_containers(self.workspace)
    }

    pub fn all_window_ids(&self) -> Vec<W::Id> {
        self.containers.all_window_ids(self.workspace)
    }

    pub fn root_children_len(&self) -> usize {
        self.containers.root_children_len(self.workspace)
    }

    pub fn debug_root_is_workspace_node(&self) -> bool {
        self.containers.debug_root_is_workspace_node(self.workspace)
    }

    pub fn contains_layout(&self, layout: Layout) -> bool {
        self.containers.contains_layout(self.workspace, layout)
    }

    pub fn active_root_index(&self) -> usize {
        self.containers.active_root_index(self.workspace)
    }

    pub fn parent_area(&self) -> Rectangle<f64, Logical> {
        self.containers.parent_area(self.workspace)
    }

    pub fn debug_selection_visual_geometry(&self) -> Option<Rectangle<f64, Logical>> {
        self.containers
            .debug_selection_visual_geometry(self.workspace)
    }

    pub fn focus_path(&self) -> Vec<usize> {
        self.containers.focus_path(self.workspace)
    }

    pub fn fullscreen_key(&self) -> Option<NodeKey> {
        self.containers.fullscreen_key(self.workspace)
    }

    pub fn fullscreen_representative_window_id(&self) -> Option<&'a W::Id> {
        self.containers
            .fullscreen_representative_window_id(self.workspace)
    }

    pub fn selected_path(&self) -> Vec<usize> {
        self.containers.selected_path(self.workspace)
    }
}

pub struct FloatingTestView<'a, W: LayoutElement> {
    pub(super) containers: &'a ContainerTree<W>,
    pub(super) workspace: NodeKey,
}

impl<'a, W: LayoutElement> FloatingTestView<'a, W> {
    pub fn tiles(&self) -> impl Iterator<Item = &'a Tile<W>> + 'a {
        self.containers.floating_tiles_in_workspace(self.workspace)
    }

    pub fn root_layout_for_window(&self, id: &W::Id) -> Option<Layout> {
        let root = self.floating_root_for_window(id)?;
        self.containers.branch_layout(root)
    }

    pub fn selected_is_container(&self, id: Option<&W::Id>) -> bool {
        let active = self.containers.active_floating_window_id(self.workspace);
        let Some(id) = id.or(active.as_ref()) else {
            return false;
        };
        let Some(key) = self.containers.window_key(id) else {
            return false;
        };
        let root = self.containers.branch_root(key);
        self.containers.selected_container_in(root)
    }

    pub fn wrapper_selected_for_window(&self, id: &W::Id) -> bool {
        self.floating_root_for_window(id)
            .is_some_and(|root| self.containers.selected_container_key() == Some(root))
    }

    pub fn is_fullscreen(&self, id: &W::Id) -> bool {
        self.containers
            .floating_fullscreen_key(self.workspace)
            .is_some()
            && self
                .containers
                .fullscreen_leaf_window_id(self.workspace)
                .is_some_and(|fullscreen| fullscreen == id)
    }

    pub fn active_window(&self) -> Option<&'a W> {
        self.containers.active_floating_window(self.workspace)
    }

    pub fn debug_tree_for_window(&self, id: &W::Id) -> Option<String>
    where
        W::Id: std::fmt::Display,
    {
        Some(
            self.containers
                .debug_branch(self.floating_root_for_window(id)?),
        )
    }

    fn floating_root_for_window(&self, id: &W::Id) -> Option<NodeKey> {
        let key = self.containers.window_key(id)?;
        (self.containers.workspace_of(key) == Some(self.workspace)
            && self.containers.is_in_floating_branch(key))
        .then(|| self.containers.branch_root(key))
    }
}
