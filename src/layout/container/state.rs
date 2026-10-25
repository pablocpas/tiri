//! View size, config, leaf-layout cache and pending-layout bookkeeping.

use std::collections::{HashMap, HashSet};
use std::rc::Rc;

use smithay::utils::{Logical, Rectangle, Size};

use super::{
    ContainerTree, LayoutElement, LayoutMode, LeafLayoutInfo, NodeData, NodeKey, WorkspaceData,
};
use crate::layout::workspace::{WorkspaceProperties, WorkspaceRuntime};
use crate::layout::Options;
use crate::utils::transaction::Transaction;

impl<W: LayoutElement> ContainerTree<W> {
    /// The box and config the tree is laid out in, read off the workspace that owns them.
    ///
    /// Every reader goes through here rather than through a field beside the tree, so that the
    /// day a second workspace root joins the tree the question "whose view size?" has one
    /// place to be answered instead of forty.
    pub(super) fn workspace_data(&self, workspace: NodeKey) -> &WorkspaceData<W> {
        self.get_workspace(workspace)
            .expect("the requested tree root is a workspace node")
    }

    pub(super) fn workspace_data_mut(&mut self, workspace: NodeKey) -> &mut WorkspaceData<W> {
        self.get_workspace_mut(workspace)
            .expect("the requested tree root is a workspace node")
    }

    pub(in crate::layout) fn view_size(&self, workspace: NodeKey) -> Size<f64, Logical> {
        self.workspace_data(workspace).view_size
    }

    pub(in crate::layout) fn working_area(&self, workspace: NodeKey) -> Rectangle<f64, Logical> {
        self.workspace_data(workspace).working_area
    }

    pub(in crate::layout) fn scale(&self, workspace: NodeKey) -> f64 {
        self.workspace_data(workspace)
            .properties
            .output_scale
            .fractional_scale()
    }

    /// What the workspace is: its name, its outputs, its own config.
    pub(in crate::layout) fn workspace_properties(
        &self,
        workspace: NodeKey,
    ) -> &WorkspaceProperties {
        &self.workspace_data(workspace).properties
    }

    pub(in crate::layout) fn workspace_properties_mut(
        &mut self,
        workspace: NodeKey,
    ) -> &mut WorkspaceProperties {
        &mut self.workspace_data_mut(workspace).properties
    }

    /// What the workspace is doing right now, as opposed to what it is.
    pub(in crate::layout) fn workspace_runtime(&self, workspace: NodeKey) -> &WorkspaceRuntime<W> {
        &self.workspace_data(workspace).runtime
    }

    pub(in crate::layout) fn workspace_runtime_mut(
        &mut self,
        workspace: NodeKey,
    ) -> &mut WorkspaceRuntime<W> {
        &mut self.workspace_data_mut(workspace).runtime
    }

    pub(in crate::layout) fn options(&self, workspace: NodeKey) -> &Rc<Options> {
        &self.workspace_data(workspace).options
    }

    /// Get the currently focused window
    pub(in crate::layout) fn focused_window(&self) -> Option<&W> {
        let key = self.seat_focused_key()?;
        self.get_tile(key).map(|tile| tile.window())
    }

    /// Update configuration
    pub(in crate::layout) fn update_config(
        &mut self,
        workspace: NodeKey,
        view_size: Size<f64, Logical>,
        working_area: Rectangle<f64, Logical>,
        options: Rc<Options>,
    ) {
        let data = self.workspace_data_mut(workspace);
        data.runtime
            .background
            .update(view_size, options.layout.background_color);
        data.view_size = view_size;
        data.working_area = working_area;
        data.options = options.clone();

        let floating_roots: Vec<_> = self.floating_roots(workspace).collect();
        for root in floating_roots {
            self.update_floating_working_area(root, working_area);
        }

        // Tile configuration is independent of the branch that currently owns the tile.
        // Walking the workspace once also guarantees a reparent cannot leave stale options
        // behind on either side.
        let scale = self.scale(workspace);
        for tile in self.tiles_mut_in_workspace(workspace) {
            tile.update_config(view_size, scale, options.clone());
        }
        self.layout(workspace);
    }

    /// Count total number of windows in tree
    pub(in crate::layout) fn window_count(&self) -> usize {
        self.workspaces
            .iter()
            .map(|workspace| self.count_windows_in_node(*workspace))
            .sum()
    }

    /// Helper: count windows in a node
    pub(super) fn count_windows_in_node(&self, node_key: NodeKey) -> usize {
        match self.get_node(node_key) {
            Some(node) if node.is_view() => 1,
            Some(NodeData::Workspace(_)) | Some(NodeData::Container(_)) => self
                .get_container(node_key)
                .expect("layout parent")
                .children
                .iter()
                .map(|&child_key| self.count_windows_in_node(child_key))
                .sum(),
            None => 0,
        }
    }

    /// The same, for one workspace: its tiled side and then its floating groups.
    pub(in crate::layout) fn leaf_layouts_in(
        &self,
        workspace: NodeKey,
    ) -> impl DoubleEndedIterator<Item = &LeafLayoutInfo> + '_ {
        self.leaf_keys_in_workspace(workspace)
            .into_iter()
            .filter_map(|key| self.leaf_layouts.get(&key))
    }

    pub(in crate::layout) fn leaf_layout(&self, key: NodeKey) -> Option<&LeafLayoutInfo> {
        self.leaf_layouts.get(&key)
    }

    pub(in crate::layout) fn leaf_layout_cache(&self) -> &HashMap<NodeKey, LeafLayoutInfo> {
        &self.leaf_layouts
    }

    /// The leaves one waiting branch describes.
    pub(in crate::layout) fn pending_leaf_layouts_for(
        &self,
        branch: NodeKey,
    ) -> Option<&HashMap<NodeKey, LeafLayoutInfo>> {
        self.pending_layouts
            .iter()
            .find(|pending| pending.branch == branch)
            .map(|pending| &pending.data.leaf_layouts)
    }

    /// Cached leaves from every layout transaction that is still waiting for clients.
    pub(in crate::layout) fn pending_leaf_layouts(
        &self,
    ) -> impl Iterator<Item = (NodeKey, &LeafLayoutInfo)> {
        self.pending_layouts.iter().flat_map(|pending| {
            self.leaf_keys_in_branch(pending.branch)
                .into_iter()
                .filter_map(move |key| {
                    pending
                        .data
                        .leaf_layouts
                        .get(&key)
                        .map(|info| (pending.branch, info))
                })
        })
    }

    pub(in crate::layout) fn set_pending_transaction(&mut self, transaction: Transaction) {
        self.pending_transaction = Some(transaction);
    }

    pub(super) fn prune_leaf_layouts(&mut self, workspace: NodeKey) {
        if self
            .seat_focused_key()
            .is_some_and(|key| !self.get_node(key).is_some_and(|node| node.is_view()))
        {
            let first = self.first_leaf_key(workspace);
            self.seat.redirect_focused_leaf(first);
        }

        if self
            .selected_key()
            .is_some_and(|key| !self.nodes.contains_key(key))
        {
            self.seat.redirect_selection(None);
        }

        self.drop_stale_leaf_layouts();
    }

    /// Forget every cached presentation whose leaf has left the tree.
    ///
    /// Separate from [`Self::prune_leaf_layouts`] because dropping the last workspace has no
    /// workspace left to redirect focus into, and the caches still have to go.
    pub(super) fn drop_stale_leaf_layouts(&mut self) {
        let live: HashSet<NodeKey> = self
            .nodes
            .keys()
            .filter(|key| self.get_node(*key).is_some_and(|node| node.is_view()))
            .collect();
        self.leaf_layouts.retain(|key, _| live.contains(key));
        for pending in &mut self.pending_layouts {
            pending
                .data
                .leaf_layouts
                .retain(|key, _| live.contains(key));
        }
    }

    /// Forget presentation state for a subtree that is transferred outside every layout branch.
    pub(super) fn drop_subtree_layouts(&mut self, root: NodeKey) {
        let leaves: HashSet<_> = self.leaf_keys_under(root).into_iter().collect();
        self.leaf_layouts.retain(|key, _| !leaves.contains(key));
        for pending in &mut self.pending_layouts {
            pending
                .data
                .leaf_layouts
                .retain(|key, _| !leaves.contains(key));
        }
    }

    /// The workspace's layout.
    ///
    /// The workspace node carries it, empty workspace included, so there is nothing to
    /// remember on the side and nothing to reconcile: a `split` on an empty workspace is a
    /// `split` on the workspace node, and the first window arriving reads the same field.
    pub(in crate::layout) fn workspace_layout(&self, workspace: NodeKey) -> LayoutMode {
        self.root_container_layout(workspace)
    }

    /// The layout carried by the workspace node.
    pub(in crate::layout) fn root_container_layout(&self, workspace: NodeKey) -> LayoutMode {
        self.get_container(workspace)
            .expect("workspace node must provide child-layout data")
            .layout()
    }

    pub(in crate::layout) fn take_pending_relayout(&mut self, workspace: NodeKey) -> bool {
        std::mem::take(&mut self.workspace_data_mut(workspace).pending_relayout)
    }

    pub(super) fn set_pending_relayout(&mut self, workspace: NodeKey, pending: bool) {
        self.workspace_data_mut(workspace).pending_relayout = pending;
    }
}
