//! View size, config, leaf-layout cache and pending-layout bookkeeping.

use std::collections::{HashMap, HashSet};
use std::rc::Rc;

use smithay::utils::{Logical, Rectangle, Size};

use super::{
    ContainerArena, Layout, LayoutElement, LeafLayoutInfo, NodeData, NodeKey, WorkspaceData,
};
use crate::layout::tile::Tile;
use crate::layout::Options;
use crate::utils::transaction::Transaction;

impl<W: LayoutElement> ContainerArena<W> {
    /// The box and config the tree is laid out in, read off the workspace that owns them.
    ///
    /// Every reader goes through here rather than through a field beside the tree, so that the
    /// day a second workspace root joins the arena the question "whose view size?" has one
    /// place to be answered instead of forty.
    fn workspace_data(&self) -> &WorkspaceData {
        self.get_workspace()
            .expect("the arena root is always a workspace node")
    }

    fn workspace_data_mut(&mut self) -> &mut WorkspaceData {
        self.get_workspace_mut()
            .expect("the arena root is always a workspace node")
    }

    pub(in crate::layout) fn view_size(&self) -> Size<f64, Logical> {
        self.workspace_data().view_size
    }

    pub(in crate::layout) fn working_area(&self) -> Rectangle<f64, Logical> {
        self.workspace_data().working_area
    }

    pub(in crate::layout) fn scale(&self) -> f64 {
        self.workspace_data().scale
    }

    pub(in crate::layout) fn options(&self) -> &Rc<Options> {
        &self.workspace_data().options
    }

    /// Get the currently focused window
    pub(in crate::layout) fn focused_window(&self) -> Option<&W> {
        let key = self.focused_key()?;
        self.get_tile(key).map(|tile| tile.window())
    }

    /// Update view size and working area
    pub(in crate::layout) fn set_view_size(
        &mut self,
        view_size: Size<f64, Logical>,
        working_area: Rectangle<f64, Logical>,
    ) {
        let workspace = self.workspace_data_mut();
        workspace.view_size = view_size;
        workspace.working_area = working_area;
    }

    /// Update configuration
    pub(in crate::layout) fn update_config(
        &mut self,
        view_size: Size<f64, Logical>,
        working_area: Rectangle<f64, Logical>,
        scale: f64,
        options: Rc<Options>,
    ) {
        let workspace = self.workspace_data_mut();
        workspace.view_size = view_size;
        workspace.working_area = working_area;
        workspace.scale = scale;
        workspace.options = options;
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

    /// Cached leaf presentations in current tree/render order.
    pub(in crate::layout) fn leaf_layouts(
        &self,
    ) -> impl DoubleEndedIterator<Item = &LeafLayoutInfo> + '_ {
        self.dfs_leaf_keys()
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
            .focused_key()
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

    /// Focused tile (if any).
    pub(in crate::layout) fn focused_tile(&self, workspace: NodeKey) -> Option<&Tile<W>> {
        let key = self
            .focused_key()
            .or_else(|| self.first_leaf_key(workspace))?;
        self.get_tile(key)
    }

    /// The workspace's layout.
    ///
    /// The workspace node carries it, empty workspace included, so there is nothing to
    /// remember on the side and nothing to reconcile: a `split` on an empty workspace is a
    /// `split` on the workspace node, and the first window arriving reads the same field.
    pub(in crate::layout) fn workspace_layout(&self, workspace: NodeKey) -> Layout {
        self.root_container_layout(workspace)
    }

    /// The layout carried by the workspace node.
    pub(in crate::layout) fn root_container_layout(&self, workspace: NodeKey) -> Layout {
        self.get_container(workspace)
            .expect("workspace node must provide child-layout data")
            .layout()
    }

    pub(in crate::layout) fn take_pending_relayout(&mut self) -> bool {
        std::mem::take(&mut self.pending_relayout)
    }
}
