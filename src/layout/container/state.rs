//! View size, config, leaf-layout cache and pending-layout bookkeeping.

use std::rc::Rc;

use smithay::utils::Logical;
use smithay::utils::Rectangle;
use smithay::utils::Size;

use super::ContainerArena;
use super::Layout;
use super::LayoutElement;
use super::LeafLayoutInfo;
use super::NodeData;
use super::NodeKey;
use crate::layout::tile::Tile;
use crate::layout::Options;
use crate::utils::transaction::Transaction;

impl<W: LayoutElement> ContainerArena<W> {
    pub(in crate::layout) fn view_size(&self) -> Size<f64, Logical> {
        self.view_size
    }

    pub(in crate::layout) fn working_area(&self) -> Rectangle<f64, Logical> {
        self.working_area
    }

    pub(in crate::layout) fn scale(&self) -> f64 {
        self.scale
    }

    pub(in crate::layout) fn options(&self) -> &Rc<Options> {
        &self.options
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
        self.view_size = view_size;
        self.working_area = working_area;
    }

    /// Update configuration
    pub(in crate::layout) fn update_config(
        &mut self,
        view_size: Size<f64, Logical>,
        working_area: Rectangle<f64, Logical>,
        scale: f64,
        options: Rc<Options>,
    ) {
        self.view_size = view_size;
        self.working_area = working_area;
        self.scale = scale;
        self.options = options;
    }

    /// Count total number of windows in tree
    pub(in crate::layout) fn window_count(&self) -> usize {
        self.count_windows_in_node(self.root)
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

    /// Access the cached leaf layout information from the last layout pass.
    pub(in crate::layout) fn leaf_layouts(&self) -> &[LeafLayoutInfo] {
        &self.leaf_layouts
    }

    /// The leaves one waiting branch describes.
    pub(in crate::layout) fn pending_leaf_layouts_for(
        &self,
        branch: NodeKey,
    ) -> Option<&[LeafLayoutInfo]> {
        self.pending_layouts
            .iter()
            .find(|pending| pending.branch == branch)
            .map(|pending| pending.data.leaf_layouts.as_slice())
    }

    /// Cached leaves from every layout transaction that is still waiting for clients.
    pub(in crate::layout) fn pending_leaf_layouts(&self) -> impl Iterator<Item = &LeafLayoutInfo> {
        self.pending_layouts
            .iter()
            .flat_map(|pending| pending.data.leaf_layouts.iter())
    }

    pub(in crate::layout) fn set_pending_transaction(&mut self, transaction: Transaction) {
        self.pending_transaction = Some(transaction);
    }

    pub(super) fn prune_leaf_layouts(&mut self) {
        if self
            .focused_key()
            .is_some_and(|key| !self.get_node(key).is_some_and(|node| node.is_view()))
        {
            let first = self.first_leaf_key();
            self.seat.redirect_focused_leaf(first);
        }

        if self
            .selected_key()
            .is_some_and(|key| !self.nodes.contains_key(key))
        {
            self.seat.redirect_selection(None);
        }

        self.readdress_leaf_layouts();
    }

    /// Focused tile (if any).
    pub(in crate::layout) fn focused_tile(&self) -> Option<&Tile<W>> {
        let key = self.focused_key().or_else(|| self.first_leaf_key())?;
        self.get_tile(key)
    }

    /// The workspace's layout.
    ///
    /// The workspace node carries it, empty workspace included, so there is nothing to
    /// remember on the side and nothing to reconcile: a `split` on an empty workspace is a
    /// `split` on the workspace node, and the first window arriving reads the same field.
    pub(in crate::layout) fn workspace_layout(&self) -> Layout {
        self.root_container_layout()
    }

    /// The layout carried by the workspace node.
    pub(in crate::layout) fn root_container_layout(&self) -> Layout {
        self.get_container(self.root)
            .expect("workspace node must provide child-layout data")
            .layout()
    }

    pub(in crate::layout) fn take_pending_relayout(&mut self) -> bool {
        std::mem::take(&mut self.pending_relayout)
    }
}
