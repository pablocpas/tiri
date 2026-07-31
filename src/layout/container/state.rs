//! View size, config, leaf-layout cache and pending-layout bookkeeping.

use std::collections::HashMap;
use std::rc::Rc;

use smithay::utils::Logical;
use smithay::utils::Rectangle;
use smithay::utils::Size;

use super::reconcile_leaf_layouts;
use super::ContainerTree;
use super::Layout;
use super::LayoutElement;
use super::LeafLayoutInfo;
use super::NodeData;
use super::NodeKey;
use crate::layout::tile::Tile;
use crate::layout::Options;
use crate::utils::transaction::Transaction;

impl<W: LayoutElement> ContainerTree<W> {
    /// Get the currently focused window
    pub(in crate::layout) fn focused_window(&self) -> Option<&W> {
        let key = self.focused_key?;
        self.get_tile(key).map(|tile| tile.window())
    }

    /// Get the currently focused window (mutable)
    pub(in crate::layout) fn focused_window_mut(&mut self) -> Option<&mut W> {
        let key = self.focused_key?;
        self.get_tile_mut(key).map(|tile| tile.window_mut())
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
        self.root
            .map_or(0, |root_key| self.count_windows_in_node(root_key))
    }

    /// Helper: count windows in a node
    pub(super) fn count_windows_in_node(&self, node_key: NodeKey) -> usize {
        match self.get_node(node_key) {
            Some(NodeData::Leaf(_)) => 1,
            Some(NodeData::Container(container)) => container
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

    /// Clone of the cached leaf layout information
    pub(in crate::layout) fn leaf_layouts_cloned(&self) -> Vec<LeafLayoutInfo> {
        self.leaf_layouts.clone()
    }

    pub(in crate::layout) fn pending_leaf_layouts(&self) -> Option<&[LeafLayoutInfo]> {
        self.pending_layouts
            .as_ref()
            .map(|pending| pending.data.leaf_layouts.as_slice())
    }

    pub(in crate::layout) fn pending_leaf_layouts_cloned(&self) -> Option<Vec<LeafLayoutInfo>> {
        self.pending_layouts
            .as_ref()
            .map(|pending| pending.data.leaf_layouts.clone())
    }

    pub(in crate::layout) fn set_pending_transaction(&mut self, transaction: Transaction) {
        self.pending_transaction = Some(transaction);
    }

    pub(super) fn prune_leaf_layouts(&mut self) {
        if self
            .focused_key
            .is_some_and(|key| !matches!(self.get_node(key), Some(NodeData::Leaf(_))))
        {
            self.focused_key = self.first_leaf_key();
        }

        if self
            .selected_key
            .is_some_and(|key| !self.nodes.contains_key(key))
        {
            self.selected_key = None;
        }

        let mut current_paths = HashMap::new();
        for key in self.nodes.keys() {
            if matches!(self.get_node(key), Some(NodeData::Leaf(_))) {
                if let Some(path) = self.find_node_path(key) {
                    current_paths.insert(key, path);
                }
            }
        }

        reconcile_leaf_layouts(&mut self.leaf_layouts, &current_paths);
        if let Some(pending) = &mut self.pending_layouts {
            reconcile_leaf_layouts(&mut pending.data.leaf_layouts, &current_paths);
        }
    }

    /// Focused tile (if any).
    pub(in crate::layout) fn focused_tile(&self) -> Option<&Tile<W>> {
        let key = self.focused_key.or_else(|| self.first_leaf_key())?;
        self.get_tile(key)
    }

    /// Focused tile (mutable) if any.
    pub(in crate::layout) fn focused_tile_mut(&mut self) -> Option<&mut Tile<W>> {
        let key = self.focused_key.or_else(|| self.first_leaf_key())?;
        self.get_tile_mut(key)
    }

    pub(in crate::layout) fn set_pending_layout(&mut self, layout: Layout) {
        self.pending_layout = Some(layout);
        // External layout hints (workspace parity plumbing) should be consumable by
        // the next split command on a single root leaf.
        self.pending_layout_wrap_on_split = true;
    }

    pub(in crate::layout) fn set_workspace_layout_hint(&mut self, layout: Layout) {
        self.pending_layout = Some(layout);
        self.pending_layout_wrap_on_split = false;
    }

    pub(in crate::layout) fn clear_pending_layout(&mut self) {
        self.pending_layout = None;
        self.pending_layout_wrap_on_split = false;
    }

    pub(in crate::layout) fn take_pending_relayout(&mut self) -> bool {
        std::mem::take(&mut self.pending_relayout)
    }
}
