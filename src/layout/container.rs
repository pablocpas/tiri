//! i3-style container tree implementation using SlotMap
//!
//! This module implements the hierarchical container system used by i3wm.
//! Containers form a tree where:
//! - Leaf nodes contain windows (wrapped in Tiles)
//! - Internal nodes contain child containers with a specific layout
//! - Each container can have layouts: SplitH, SplitV, Tabbed, or Stacked
//!
//! Uses slotmap for efficient memory management and O(1) access to nodes.

use std::cell::RefCell;
use std::collections::HashMap;
use std::rc::Rc;

use slotmap::{new_key_type, SecondaryMap, SlotMap};
use smithay::utils::{Logical, Rectangle, Size};

use super::tile::Tile;
use super::{LayoutElement, Options};
use crate::utils::transaction::Transaction;
use tiri_config::BlockOutFrom;

mod cleanup;
mod command;
mod debug;
mod detach;
mod focus;
mod geometry;
mod inactive_reference;
mod insert;
mod invariants;
mod ipc_projection;
mod movement;
mod paths;
mod preview;
mod query;
mod resize;
mod root_children;
mod split;
mod state;
mod tab_bar_model;
mod tree_store;

pub(super) use command::RootPolicy;
pub(super) use command::TreeCommandTarget;
use geometry::PendingLayout;
pub(super) use resize::ResizeTarget;

// ============================================================================
// SlotMap Key Types
// ============================================================================

new_key_type! {
    /// Key to reference a node in the container tree
    pub struct NodeKey;
}

// ============================================================================
// Container Types and Enums
// ============================================================================

/// Layout mode for a container (following i3 model)
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub enum Layout {
    /// Horizontal split - children arranged left to right
    #[default]
    SplitH,
    /// Vertical split - children arranged top to bottom
    SplitV,
    /// Tabbed layout - children stacked with tab bar
    Tabbed,
    /// Stacked layout - children stacked with title bars
    Stacked,
}

/// Which siblings a resize takes the space from.
///
/// sway 1.12's axis form reaches every sibling, while an edge form reaches only the sibling
/// on that edge. The split between the payers, the floor, and the ancestor the resize climbs
/// to are otherwise the same.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ResizeReach {
    /// `resize grow width|height`: every sibling except the resized child.
    Siblings,
    /// `resize grow left|up`: the one before.
    Before,
    /// `resize grow right|down`: the one after.
    After,
}

/// Pixel geometry needed to reproduce sway's tiled-resize preflight.
#[derive(Debug, Clone, Copy)]
pub(in crate::layout) struct ResizeSpace {
    pub available: f64,
    pub min_size: f64,
}

/// Direction for navigation and movement
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Direction {
    Left,
    Right,
    Up,
    Down,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TabBarTab {
    pub title: String,
    pub is_focused: bool,
    pub is_urgent: bool,
    pub block_out_from: Option<BlockOutFrom>,
}

#[derive(Debug, Clone)]
pub struct TabBarInfo {
    /// The container this bar belongs to.
    pub key: NodeKey,
    /// Its path, kept for the render cache key and the IPC-facing consumers.
    pub path: Vec<usize>,
    pub layout: Layout,
    pub rect: Rectangle<f64, Logical>,
    pub row_height: f64,
    pub tabs: Vec<TabBarTab>,
}

const MIN_CHILD_PERCENT: f64 = 0.05;

/// Node type in the container tree
// Tile<W> dwarfs ContainerData, but trees hold one node per window; boxing the
// tile would add an indirection on every render-path access for negligible savings.
#[allow(clippy::large_enum_variant)]
#[derive(Debug)]
pub enum NodeData<W: LayoutElement> {
    /// Container node with children (stored as keys)
    Container(ContainerData),
    /// Leaf node containing a tile
    Leaf(Tile<W>),
}

/// Detached subtree used to move container structures across trees.
#[allow(clippy::large_enum_variant)]
#[derive(Debug)]
pub enum DetachedNode<W: LayoutElement> {
    Container(DetachedContainer<W>),
    Leaf(Tile<W>),
}

#[derive(Debug)]
pub struct DetachedContainer<W: LayoutElement> {
    layout: Layout,
    children: Vec<DetachedNode<W>>,
    child_percents: Vec<f64>,
    focus_stack: Vec<usize>,
    user_created: bool,
    prev_split_layout: Option<Layout>,
}

/// Container data stored in slotmap
#[derive(Debug)]
pub struct ContainerData {
    /// Layout mode for this container
    layout: Layout,
    /// Child node keys (indices into the tree's SlotMap)
    children: Vec<NodeKey>,
    /// Focus history (most recently used first)
    focus_stack: Vec<NodeKey>,
    /// Preserve container even if it has a single child (explicit split).
    user_created: bool,
    /// Previous split layout for i3-style `layout toggle split`.
    prev_split_layout: Option<Layout>,
    /// Relative sizes of children (sum normalized to 1.0 for split layouts)
    child_percents: Vec<f64>,
    /// Cached geometry for rendering
    geometry: Rectangle<f64, Logical>,
}

/// Cached layout information for a leaf tile.
#[derive(Debug, Clone)]
pub struct LeafLayoutInfo {
    pub key: NodeKey,
    pub path: Vec<usize>,
    pub rect: Rectangle<f64, Logical>,
    pub visible: bool,
}

#[derive(Debug, Clone)]
pub(super) struct InsertParentInfo {
    pub parent_path: Vec<usize>,
    pub insert_idx: usize,
    pub layout: Layout,
    pub child_percents: Vec<f64>,
}

/// Subtree detached from a tree along with its origin info and geometry.
pub(super) type TakenSubtree<W> = (
    DetachedNode<W>,
    Option<InsertParentInfo>,
    Rectangle<f64, Logical>,
);

/// Container key, child index, available span, child count and rect of a window's container.
pub(super) type ContainerMetrics = (NodeKey, usize, f64, usize, Rectangle<f64, Logical>);

#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) enum InactiveTilingReference {
    Leaf { key: NodeKey, path_hint: Vec<usize> },
    Container { key: NodeKey, path_hint: Vec<usize> },
}

impl InactiveTilingReference {
    pub(super) fn node_key(&self) -> NodeKey {
        match self {
            InactiveTilingReference::Leaf { key, .. } => *key,
            InactiveTilingReference::Container { key, .. } => *key,
        }
    }
}

/// A reference resolved against the live tree: the node it names, plus its path for the
/// path-shaped `InsertParentInfo` the restore path still speaks.
enum ResolvedInactiveTilingReference {
    Leaf { key: NodeKey, path: Vec<usize> },
    Container { key: NodeKey, path: Vec<usize> },
}

/// Root container tree for a workspace
#[derive(Debug)]
pub struct ContainerTree<W: LayoutElement> {
    /// SlotMap storing all nodes in the tree
    nodes: SlotMap<NodeKey, NodeData<W>>,
    /// Parent pointer for each node (None for root)
    parents: SecondaryMap<NodeKey, Option<NodeKey>>,
    /// The workspace.
    ///
    /// Always present and always a container, empty tree included.
    ///
    /// sway keeps `sway_workspace` and `sway_container` as separate structs, but the
    /// workspace carries the same `layout` and `prev_split_layout` and answers the same
    /// questions — it is a container's worth of state under another type, which is why i3,
    /// where a workspace is an ordinary `Con`, gets the same answers out of one code path.
    /// Modelling it as a node here buys that same uniformity: every rule that used to ask
    /// "is the parent the workspace?" was working around its absence, and each of those
    /// workarounds disagreed with sway somewhere.
    root: NodeKey,
    /// Focused leaf node key (source of truth for focus).
    focused_key: Option<NodeKey>,
    /// Currently selected node key (container selection via focus-parent).
    selected_key: Option<NodeKey>,
    /// Cached layout info for leaves
    leaf_layouts: Vec<LeafLayoutInfo>,
    /// Pending layouts waiting for transactions to complete.
    pending_layouts: Option<PendingLayout>,
    /// Optional transaction to use for the next atomic layout.
    pending_transaction: Option<Transaction>,
    /// Whether a new layout is requested while a transaction is pending.
    pending_relayout: bool,
    /// View size (output size)
    view_size: Size<f64, Logical>,
    /// Working area (view_size minus gaps/bars)
    working_area: Rectangle<f64, Logical>,
    /// Display scale
    scale: f64,
    /// Layout options
    options: Rc<Options>,
    /// Generation counter for cache invalidation.
    generation: u64,
    /// Cached focus path to avoid recomputation (generation, focused_key, path).
    focus_path_cache: RefCell<(u64, Option<NodeKey>, Vec<usize>)>,
}

#[derive(Debug, Clone, Copy)]
pub(super) struct PreviewLeafGeometry {
    pub rect: Rectangle<f64, Logical>,
    pub tab_bar_offset: f64,
}

// ============================================================================
// ContainerData Implementation
// ============================================================================

impl ContainerData {
    /// Create a new container with given layout
    pub(super) fn new(layout: Layout) -> Self {
        Self {
            layout,
            children: Vec::new(),
            focus_stack: Vec::new(),
            user_created: false,
            prev_split_layout: None,
            child_percents: Vec::new(),
            geometry: Rectangle::from_size(Size::from((0.0, 0.0))),
        }
    }

    /// Get container layout
    pub(super) fn layout(&self) -> Layout {
        self.layout
    }

    /// Set container layout
    pub(super) fn set_layout(&mut self, layout: Layout) {
        if self.layout != layout && matches!(self.layout, Layout::SplitH | Layout::SplitV) {
            self.prev_split_layout = Some(self.layout);
        }
        self.layout = layout;
    }

    pub(super) fn set_layout_explicit(&mut self, layout: Layout) {
        self.set_layout(layout);
        self.user_created = true;
    }

    /// Whether this container is one the user asked for, rather than scaffolding.
    ///
    /// `focus parent` stops at one, a floating wrapper is selectable when it is one, and a
    /// split is addressable when it is one. The bit used to carry a second, unrelated
    /// meaning — "cleanup must not dissolve me" — which was approximating a rule about what
    /// a command had just done rather than anything about the container. See the
    /// `preserve_on_single` section of `docs/design/parity.md`.
    pub(super) fn is_user_container(&self) -> bool {
        self.user_created
    }

    pub(super) fn prev_split_layout(&self) -> Option<Layout> {
        self.prev_split_layout
    }

    /// Mark this container as one the user asked for.
    pub(super) fn mark_user_created(&mut self) {
        self.user_created = true;
    }

    /// Get children keys
    pub(super) fn children(&self) -> &[NodeKey] {
        &self.children
    }

    /// Number of children
    pub(super) fn child_count(&self) -> usize {
        self.children.len()
    }

    /// Get focused child key
    pub(super) fn focused_child_key(&self) -> Option<NodeKey> {
        self.focus_stack
            .first()
            .copied()
            .or_else(|| self.children.first().copied())
    }

    pub(super) fn focused_child_index(&self) -> Option<usize> {
        let key = self.focused_child_key()?;
        self.children.iter().position(|child| *child == key)
    }

    /// Replace `old_key` with `new_key` in place, keeping its child slot, size share and
    /// focus-stack position.
    pub(super) fn replace_child_preserving_focus(
        &mut self,
        old_key: NodeKey,
        new_key: NodeKey,
    ) -> bool {
        let Some(idx) = self.children.iter().position(|key| *key == old_key) else {
            return false;
        };
        self.children[idx] = new_key;
        if let Some(pos) = self.focus_stack.iter().position(|key| *key == old_key) {
            self.focus_stack[pos] = new_key;
        } else if !self.focus_stack.contains(&new_key) {
            self.focus_stack.push(new_key);
        }
        self.ensure_focus_stack();
        true
    }

    pub(super) fn bubble_focus(&mut self, node_key: NodeKey) {
        self.ensure_focus_stack();
        if let Some(pos) = self.focus_stack.iter().position(|key| *key == node_key) {
            self.focus_stack.remove(pos);
        }
        self.focus_stack.insert(0, node_key);
    }

    fn ensure_focus_stack(&mut self) {
        self.focus_stack.retain(|key| self.children.contains(key));
        for child in &self.children {
            if !self.focus_stack.contains(child) {
                self.focus_stack.push(*child);
            }
        }
    }

    /// Add a child node by key
    pub(super) fn add_child(&mut self, node_key: NodeKey) {
        let idx = self.children.len();
        self.insert_child(idx, node_key);
    }

    /// Remove a child at index, returns the removed node key
    pub(super) fn remove_child(&mut self, idx: usize) -> Option<NodeKey> {
        if idx >= self.children.len() {
            return None;
        }

        let key = self.children.remove(idx);
        self.focus_stack.retain(|child| *child != key);
        let removed_percent = if self.child_percents.len() == self.children.len() + 1 {
            self.child_percents.remove(idx)
        } else {
            0.0
        };

        if self.children.is_empty() {
            self.child_percents.clear();
            self.focus_stack.clear();
            return Some(key);
        }

        if self.child_percents.len() != self.children.len() {
            self.recalculate_percentages();
            self.ensure_focus_stack();
            return Some(key);
        }

        let remaining = 1.0 - removed_percent;
        if remaining > f64::EPSILON {
            let scale = 1.0 / remaining;
            for percent in &mut self.child_percents {
                *percent *= scale;
            }
            self.normalize_child_percents();
        } else {
            self.recalculate_percentages();
        }

        self.ensure_focus_stack();
        Some(key)
    }

    /// Get child key at index
    pub(super) fn child_key(&self, idx: usize) -> Option<NodeKey> {
        self.children.get(idx).copied()
    }

    pub(super) fn insert_child(&mut self, idx: usize, node_key: NodeKey) {
        let idx = idx.min(self.children.len());
        let old_len = self.children.len();

        if old_len == 0 {
            self.children.insert(idx, node_key);
            self.focus_stack.push(node_key);
            self.child_percents.clear();
            self.child_percents.push(1.0);
            return;
        }

        if self.child_percents.len() != old_len {
            self.child_percents.clear();
            let value = 1.0 / old_len as f64;
            self.child_percents.resize(old_len, value);
        } else {
            self.normalize_child_percents();
        }

        let new_share = 1.0 / (old_len as f64 + 1.0);
        let scale = 1.0 - new_share;
        for percent in &mut self.child_percents {
            *percent *= scale;
        }

        self.children.insert(idx, node_key);
        self.child_percents.insert(idx, new_share);
        self.normalize_child_percents();
        if !self.focus_stack.contains(&node_key) {
            self.focus_stack.push(node_key);
        }
    }

    pub(super) fn recalculate_percentages(&mut self) {
        if self.children.is_empty() {
            self.child_percents.clear();
            return;
        }
        let count = self.children.len() as f64;
        let value = 1.0 / count;
        if self.child_percents.len() != self.children.len() {
            self.child_percents.resize(self.children.len(), value);
        }
        for percent in &mut self.child_percents {
            *percent = value;
        }
    }

    /// Resolve the stored shares the way sway's `arrange` does, in place.
    ///
    /// Run at the end of a command, never during one: sway invalidates fractions while the
    /// tree moves and only fills them in when it arranges, so filling early answers the
    /// question with the siblings a mid-command tree happens to have.
    pub(super) fn resolve_child_percents(&mut self) {
        self.child_percents = resolved_percents(&self.child_percents, self.children.len());
    }

    /// Mark a child's share unset, sway's `width_fraction = height_fraction = 0`.
    ///
    /// The others are left exactly as they are — the point is that this child's share is not
    /// known yet, not that anyone else's changed.
    pub(super) fn unset_child_percent(&mut self, idx: usize) {
        if self.child_percents.len() != self.children.len() {
            self.recalculate_percentages();
        }
        if let Some(percent) = self.child_percents.get_mut(idx) {
            *percent = 0.0;
        }
    }

    pub(super) fn normalize_child_percents(&mut self) {
        if self.child_percents.is_empty() {
            return;
        }
        let mut sum = 0.0;
        for percent in &self.child_percents {
            if !percent.is_finite() || *percent < 0.0 {
                sum = 0.0;
                break;
            }
            sum += *percent;
        }
        if sum <= f64::EPSILON {
            self.recalculate_percentages();
            return;
        }
        for percent in &mut self.child_percents {
            *percent /= sum;
        }
    }

    pub(super) fn child_percent(&self, idx: usize) -> f64 {
        self.child_percents.get(idx).copied().unwrap_or(0.0)
    }

    /// Get child percentages as a slice (avoids cloning)
    pub(super) fn child_percents_slice(&self) -> &[f64] {
        &self.child_percents
    }

    pub(super) fn set_child_percent(&mut self, idx: usize, percent: f64) {
        if self.child_percents.len() != self.children.len() {
            self.recalculate_percentages();
        }

        if self.child_percents.is_empty() || idx >= self.child_percents.len() {
            return;
        }

        let len = self.child_percents.len();
        if len == 1 {
            self.child_percents[0] = 1.0;
            return;
        }

        let min = MIN_CHILD_PERCENT;
        let max = 1.0 - min * (len as f64 - 1.0);
        let new_percent = percent.clamp(min, max.max(min));

        self.child_percents[idx] = new_percent;

        let mut remaining = 1.0 - new_percent;
        if remaining <= f64::EPSILON {
            remaining = min * (len as f64 - 1.0);
        }

        let mut others_sum = 0.0;
        for (i, value) in self.child_percents.iter().enumerate() {
            if i != idx {
                others_sum += *value;
            }
        }

        if others_sum <= f64::EPSILON {
            let share = remaining / (len as f64 - 1.0);
            for (i, value) in self.child_percents.iter_mut().enumerate() {
                if i != idx {
                    *value = share;
                }
            }
        } else {
            let scale = remaining / others_sum;
            for (i, value) in self.child_percents.iter_mut().enumerate() {
                if i != idx {
                    *value *= scale;
                }
            }
        }

        self.normalize_child_percents();
    }

    /// sway's `container_resize_tiled`: move `amount` of this container's extent into the
    /// child at `idx`, taken in equal parts from the siblings selected by `reach`.
    ///
    /// Which children pay is the only thing that differs between the forms of `resize`: an
    /// sway 1.12 changed the axis form to reach every sibling; an edge still reaches one.
    /// Everything after that is the same — the equal split, and the floor that abandons the
    /// whole change rather than part of it — which is why the reach is a parameter and there
    /// is one function instead of four.
    ///
    /// Equal parts, not proportional ones: a sibling twice the size of another still pays the
    /// same. `available` and `min_size` keep sway's floor in pixels: its preflight check
    /// rounds each payer's cost up to a whole pixel even though the applied fraction keeps
    /// the exact equal split.
    pub(super) fn resize_child(
        &mut self,
        idx: usize,
        reach: ResizeReach,
        amount: f64,
        space: ResizeSpace,
    ) -> bool {
        if self.child_percents.len() != self.children.len() {
            self.recalculate_percentages();
        }

        let len = self.child_percents.len();
        if idx >= len {
            return false;
        }

        let mut payers = Vec::with_capacity(len.saturating_sub(1));
        match reach {
            ResizeReach::Siblings => payers.extend((0..len).filter(|candidate| *candidate != idx)),
            ResizeReach::Before if idx > 0 => payers.push(idx - 1),
            ResizeReach::After if idx + 1 < len => payers.push(idx + 1),
            ResizeReach::Before | ResizeReach::After => {}
        }
        let Some(each) = (!payers.is_empty()).then(|| amount / payers.len() as f64) else {
            return false;
        };

        if space.available <= 0.0 {
            return false;
        }
        let amount_size = amount * space.available;
        let payer_check_size = (amount_size / payers.len() as f64).ceil();

        // Nothing at all rather than something lopsided: sway checks every share first and
        // abandons the resize if any of them would not fit, which is why dragging a window
        // into a wall stops instead of squashing the neighbour past it.
        if self.child_percents[idx] * space.available + amount_size < space.min_size {
            return false;
        }
        if payers.iter().any(|payer| {
            self.child_percents[*payer] * space.available - payer_check_size < space.min_size
        }) {
            return false;
        }

        self.child_percents[idx] += amount;
        for payer in payers {
            self.child_percents[payer] -= each;
        }
        true
    }

    pub(super) fn set_child_percent_pair(
        &mut self,
        idx: usize,
        neighbor_idx: usize,
        percent: f64,
    ) -> bool {
        if self.child_percents.len() != self.children.len() {
            self.recalculate_percentages();
        }

        let len = self.child_percents.len();
        if len < 2 || idx >= len || neighbor_idx >= len || idx == neighbor_idx {
            return false;
        }

        let total = self.child_percents[idx] + self.child_percents[neighbor_idx];
        if total <= f64::EPSILON {
            return false;
        }

        let min = MIN_CHILD_PERCENT;
        if total < min * 2.0 {
            return false;
        }

        let max_target = total - min;
        let new_percent = percent.clamp(min, max_target);
        let neighbor_percent = total - new_percent;

        if (self.child_percents[idx] - new_percent).abs() <= f64::EPSILON
            && (self.child_percents[neighbor_idx] - neighbor_percent).abs() <= f64::EPSILON
        {
            return false;
        }

        self.child_percents[idx] = new_percent;
        self.child_percents[neighbor_idx] = neighbor_percent;
        true
    }

    /// Set geometry for this container
    pub(super) fn set_geometry(&mut self, geometry: Rectangle<f64, Logical>) {
        self.geometry = geometry;
    }

    /// Get geometry
    pub(super) fn geometry(&self) -> Rectangle<f64, Logical> {
        self.geometry
    }
}

// ============================================================================
// Detached subtree helpers
// ============================================================================

impl<W: LayoutElement> DetachedNode<W> {
    pub(super) fn tiles(&self) -> Vec<&Tile<W>> {
        let mut tiles = Vec::new();
        self.collect_tiles(&mut tiles);
        tiles
    }

    fn collect_tiles<'a>(&'a self, tiles: &mut Vec<&'a Tile<W>>) {
        match self {
            DetachedNode::Leaf(tile) => tiles.push(tile),
            DetachedNode::Container(container) => {
                for child in &container.children {
                    child.collect_tiles(tiles);
                }
            }
        }
    }

    pub(super) fn contains_window(&self, window_id: &W::Id) -> bool {
        match self {
            DetachedNode::Leaf(tile) => tile.window().id() == window_id,
            DetachedNode::Container(container) => container
                .children
                .iter()
                .any(|child| child.contains_window(window_id)),
        }
    }

    pub(super) fn into_tiles(self) -> Vec<Tile<W>> {
        let mut tiles = Vec::new();
        self.collect_tiles_owned(&mut tiles);
        tiles
    }

    /// Drop a single implicit split wrapper at subtree root.
    ///
    /// Floating containers are internally represented as root split containers even for
    /// a single window. When moving such a subtree back to tiling we must not materialize
    /// that implicit wrapper, otherwise tiling gains an extra one-child split unlike i3/sway.
    pub(super) fn collapse_implicit_single_child_split_root(self) -> Self {
        match self {
            DetachedNode::Container(mut container)
                if !container.user_created
                    && container.children.len() == 1
                    && matches!(container.layout, Layout::SplitH | Layout::SplitV) =>
            {
                container.children.remove(0)
            }
            other => other,
        }
    }

    fn collect_tiles_owned(self, tiles: &mut Vec<Tile<W>>) {
        match self {
            DetachedNode::Leaf(tile) => tiles.push(tile),
            DetachedNode::Container(container) => {
                for child in container.children {
                    child.collect_tiles_owned(tiles);
                }
            }
        }
    }

    pub(super) fn for_each_tile_mut(&mut self, f: &mut impl FnMut(&mut Tile<W>)) {
        match self {
            DetachedNode::Leaf(tile) => f(tile),
            DetachedNode::Container(container) => {
                for child in &mut container.children {
                    child.for_each_tile_mut(f);
                }
            }
        }
    }
}

impl<W: LayoutElement> DetachedContainer<W> {
    pub(super) fn new(layout: Layout, children: Vec<DetachedNode<W>>) -> Self {
        let mut container = Self {
            layout,
            children,
            child_percents: Vec::new(),
            focus_stack: Vec::new(),
            user_created: false,
            prev_split_layout: None,
        };
        container.ensure_focus_stack();
        container.recalculate_percentages();
        container
    }

    pub(crate) fn from_parts(
        layout: Layout,
        children: Vec<DetachedNode<W>>,
        child_percents: Vec<f64>,
        focus_stack: Vec<usize>,
        user_created: bool,
        prev_split_layout: Option<Layout>,
    ) -> Self {
        let mut container = Self {
            layout,
            children,
            child_percents,
            focus_stack,
            user_created,
            prev_split_layout,
        };
        container.normalize_child_percents();
        container.ensure_focus_stack();
        container
    }

    fn recalculate_percentages(&mut self) {
        if self.children.is_empty() {
            self.child_percents.clear();
            return;
        }
        let count = self.children.len() as f64;
        let value = 1.0 / count;
        self.child_percents.clear();
        self.child_percents.resize(self.children.len(), value);
    }

    fn normalize_child_percents(&mut self) {
        if self.child_percents.len() != self.children.len() {
            self.recalculate_percentages();
            return;
        }
        if self.child_percents.is_empty() {
            return;
        }
        let mut sum = 0.0;
        for percent in &self.child_percents {
            if !percent.is_finite() || *percent < 0.0 {
                sum = 0.0;
                break;
            }
            sum += *percent;
        }
        if sum <= f64::EPSILON {
            self.recalculate_percentages();
            return;
        }
        for percent in &mut self.child_percents {
            *percent /= sum;
        }
    }

    fn ensure_focus_stack(&mut self) {
        self.focus_stack.retain(|idx| *idx < self.children.len());
        let mut seen = vec![false; self.children.len()];
        self.focus_stack.retain(|idx| {
            if seen[*idx] {
                false
            } else {
                seen[*idx] = true;
                true
            }
        });
        for (idx, seen) in seen.iter().enumerate() {
            if !seen {
                self.focus_stack.push(idx);
            }
        }
    }
}

// ============================================================================
// ContainerTree Implementation
// ============================================================================

impl<W: LayoutElement> ContainerTree<W> {
    /// Create a new empty container tree
    pub(super) fn new(
        view_size: Size<f64, Logical>,
        working_area: Rectangle<f64, Logical>,
        scale: f64,
        options: Rc<Options>,
    ) -> Self {
        let mut nodes = SlotMap::with_key();
        let mut parents = SecondaryMap::new();
        let root = nodes.insert(NodeData::Container(ContainerData::new(Layout::SplitH)));
        parents.insert(root, None);

        Self {
            nodes,
            parents,
            root,
            focused_key: None,
            selected_key: None,
            leaf_layouts: Vec::new(),
            pending_layouts: None,
            pending_transaction: None,
            pending_relayout: false,
            view_size,
            working_area,
            scale,
            options,
            generation: 0,
            focus_path_cache: RefCell::new((u64::MAX, None, Vec::new())),
        }
    }

    /// Size share of the `child_idx`-th child of the container at `container_key`.
    pub(in crate::layout) fn child_percent(
        &self,
        container_key: NodeKey,
        child_idx: usize,
    ) -> Option<f64> {
        let container = self.get_container(container_key)?;
        if child_idx >= container.child_count() {
            return None;
        }
        Some(container.child_percent(child_idx))
    }

    /// Move `amount` of `container_key`'s extent into its `child_idx`-th child, provided the
    /// container still splits along `layout`'s axis.
    pub(in crate::layout) fn resize_child(
        &mut self,
        container_key: NodeKey,
        child_idx: usize,
        layout: Layout,
        reach: ResizeReach,
        amount: f64,
        space: ResizeSpace,
    ) -> bool {
        let Some(container) = self.get_container_mut(container_key) else {
            return false;
        };
        if container.layout() != layout || child_idx >= container.child_count() {
            return false;
        }
        container.resize_child(child_idx, reach, amount, space)
    }

    /// Give the `child_idx`-th child of `container_key` a `percent` share, provided the
    /// container still splits along `layout`'s axis.
    pub(in crate::layout) fn set_child_percent(
        &mut self,
        container_key: NodeKey,
        child_idx: usize,
        layout: Layout,
        percent: f64,
    ) -> bool {
        let Some(container) = self.get_container_mut(container_key) else {
            return false;
        };
        if container.layout() != layout || child_idx >= container.child_count() {
            return false;
        }
        container.set_child_percent(child_idx, percent);
        true
    }
}

fn reconcile_leaf_layouts(
    layouts: &mut Vec<LeafLayoutInfo>,
    current_paths: &HashMap<NodeKey, Vec<usize>>,
) {
    layouts.retain_mut(|info| {
        let Some(path) = current_paths.get(&info.key) else {
            return false;
        };
        info.path.clone_from(path);
        true
    });
}

// ============================================================================
// Additional helper implementations
// ============================================================================

impl Layout {
    /// Whether children of a container with this layout are arranged left to right. Tabs
    /// count as horizontal and stacks as vertical: their titles run that way, and so does
    /// every question about them that has an axis in it.
    pub fn is_horizontal(self) -> bool {
        matches!(self, Layout::SplitH | Layout::Tabbed)
    }

    /// Whether children of a container with this layout are arranged along `direction`'s axis,
    /// so that moving or focusing in that direction steps between siblings.
    pub fn is_parallel_to(self, direction: Direction) -> bool {
        self.is_horizontal() == direction.is_horizontal()
    }

    /// Whether two layouts arrange their children along the same axis.
    pub fn is_parallel_to_layout(self, other: Layout) -> bool {
        self.is_horizontal() == other.is_horizontal()
    }

    /// Next layout in sway's `layout toggle all` cycle:
    /// SplitH -> SplitV -> Stacked -> Tabbed -> SplitH.
    pub fn next_in_cycle(self) -> Layout {
        match self {
            Layout::SplitH => Layout::SplitV,
            Layout::SplitV => Layout::Stacked,
            Layout::Stacked => Layout::Tabbed,
            Layout::Tabbed => Layout::SplitH,
        }
    }
}

impl Direction {
    /// Get the opposite direction
    pub fn opposite(self) -> Self {
        match self {
            Direction::Left => Direction::Right,
            Direction::Right => Direction::Left,
            Direction::Up => Direction::Down,
            Direction::Down => Direction::Up,
        }
    }

    /// Check if direction is horizontal
    pub fn is_horizontal(self) -> bool {
        matches!(self, Direction::Left | Direction::Right)
    }

    /// Check if direction is vertical
    pub fn is_vertical(self) -> bool {
        matches!(self, Direction::Up | Direction::Down)
    }

    /// The split layout whose axis runs along this direction.
    pub fn split_layout(self) -> Layout {
        if self.is_horizontal() {
            Layout::SplitH
        } else {
            Layout::SplitV
        }
    }

    /// Whether this direction points at the start of its axis (left/up) rather than the end.
    pub fn is_leading(self) -> bool {
        matches!(self, Direction::Left | Direction::Up)
    }

    /// Index of the sibling adjacent to `idx` in this direction, if it exists.
    pub fn sibling_index(self, idx: usize, count: usize) -> Option<usize> {
        if self.is_leading() {
            idx.checked_sub(1)
        } else {
            (idx + 1 < count).then_some(idx + 1)
        }
    }
}

/// sway's `apply_horiz_layout`/`apply_vert_layout`, which is the only place a size share is
/// decided.
///
/// A child whose fraction is unset — zero, which is what `cmd_move` writes over the ones it
/// disturbs — takes the average of the children that still have one, and then the whole list
/// is normalized to sum to 1. Nothing else is consulted: not how many children there are, not
/// what the unset one used to hold. That average is why a container emptied by a move lands
/// on exactly `1/n`, and why three of the four size divergences in the corpus were tiri
/// deciding a share at the moment of the mutation instead of leaving it to be filled in.
pub(super) fn resolved_percents(percents: &[f64], count: usize) -> Vec<f64> {
    if count == 0 {
        return Vec::new();
    }
    let even = vec![1.0 / count as f64; count];
    if percents.len() != count {
        return even;
    }

    let mut resolved: Vec<f64> = percents
        .iter()
        .map(|percent| {
            if percent.is_finite() && *percent > 0.0 {
                *percent
            } else {
                0.0
            }
        })
        .collect();

    let set = resolved.iter().filter(|percent| **percent > 0.0).count();
    let known: f64 = resolved.iter().sum();
    if set == 0 {
        return even;
    }
    let average = known / set as f64;
    for percent in &mut resolved {
        if *percent <= 0.0 {
            *percent = average;
        }
    }

    let total: f64 = resolved.iter().sum();
    if total <= f64::EPSILON {
        return even;
    }
    for percent in &mut resolved {
        *percent /= total;
    }
    resolved
}

#[cfg(test)]
fn layout_label(layout: Layout) -> &'static str {
    match layout {
        Layout::SplitH => "SplitH",
        Layout::SplitV => "SplitV",
        Layout::Tabbed => "Tabbed",
        Layout::Stacked => "Stacked",
    }
}
