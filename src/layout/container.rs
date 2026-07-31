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
mod root_children;
mod split;
mod state;
mod tab_bar_model;
mod tree_store;

pub(super) use command::RootPolicy;
use command::TreeCommandTarget;
use geometry::PendingLayout;

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
    preserve_on_single: bool,
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
    preserve_on_single: bool,
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

/// Parent path, child index, available span, child count and rect of a window's container.
pub(super) type ContainerMetrics = (Vec<usize>, usize, f64, usize, Rectangle<f64, Logical>);

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

enum ResolvedInactiveTilingReference {
    Leaf { path: Vec<usize> },
    Container { key: NodeKey, path: Vec<usize> },
}

/// Root container tree for a workspace
#[derive(Debug)]
pub struct ContainerTree<W: LayoutElement> {
    /// SlotMap storing all nodes in the tree
    nodes: SlotMap<NodeKey, NodeData<W>>,
    /// Parent pointer for each node (None for root)
    parents: SecondaryMap<NodeKey, Option<NodeKey>>,
    /// Root node key
    root: Option<NodeKey>,
    /// Layout to apply when the tree is empty (i3 workspace_layout equivalent).
    pending_layout: Option<Layout>,
    /// Whether pending_layout should be consumed by the next split on a root leaf.
    /// This is used for i3/sway semantics after `layout split*` on a single tiled leaf.
    pending_layout_wrap_on_split: bool,
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
            preserve_on_single: false,
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
        self.preserve_on_single = true;
    }

    pub(super) fn preserve_on_single(&self) -> bool {
        self.preserve_on_single
    }

    pub(super) fn prev_split_layout(&self) -> Option<Layout> {
        self.prev_split_layout
    }

    pub(super) fn mark_preserve_on_single(&mut self) {
        self.preserve_on_single = true;
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
                if !container.preserve_on_single
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
            preserve_on_single: false,
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
        preserve_on_single: bool,
        prev_split_layout: Option<Layout>,
    ) -> Self {
        let mut container = Self {
            layout,
            children,
            child_percents,
            focus_stack,
            preserve_on_single,
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
        Self {
            nodes: SlotMap::with_key(),
            parents: SecondaryMap::new(),
            root: None,
            pending_layout: None,
            pending_layout_wrap_on_split: false,
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

    pub(super) fn child_percent_at(&self, parent_path: &[usize], child_idx: usize) -> Option<f64> {
        let container_key = if parent_path.is_empty() {
            self.root?
        } else {
            self.get_node_key_at_path(parent_path)?
        };

        let container = self.get_container(container_key)?;

        if child_idx >= container.child_count() {
            return None;
        }
        Some(container.child_percent(child_idx))
    }

    pub(super) fn set_child_percent_at(
        &mut self,
        parent_path: &[usize],
        child_idx: usize,
        layout: Layout,
        percent: f64,
    ) -> bool {
        let container_key = if parent_path.is_empty() {
            match self.root {
                Some(key) => key,
                None => return false,
            }
        } else {
            match self.get_node_key_at_path(parent_path) {
                Some(key) => key,
                None => return false,
            }
        };

        if let Some(container) = self.get_container_mut(container_key) {
            if container.layout() != layout || child_idx >= container.child_count() {
                return false;
            }
            container.set_child_percent(child_idx, percent);
            true
        } else {
            false
        }
    }

    pub(super) fn set_child_percent_pair_at(
        &mut self,
        parent_path: &[usize],
        child_idx: usize,
        neighbor_idx: usize,
        layout: Layout,
        percent: f64,
    ) -> bool {
        let container_key = if parent_path.is_empty() {
            match self.root {
                Some(key) => key,
                None => return false,
            }
        } else {
            match self.get_node_key_at_path(parent_path) {
                Some(key) => key,
                None => return false,
            }
        };

        if let Some(container) = self.get_container_mut(container_key) {
            if container.layout() != layout
                || child_idx >= container.child_count()
                || neighbor_idx >= container.child_count()
            {
                return false;
            }
            container.set_child_percent_pair(child_idx, neighbor_idx, percent)
        } else {
            false
        }
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
    /// Whether children of a container with this layout are arranged along `direction`'s axis,
    /// so that moving or focusing in that direction steps between siblings.
    pub fn is_parallel_to(self, direction: Direction) -> bool {
        match self {
            Layout::SplitH | Layout::Tabbed => direction.is_horizontal(),
            Layout::SplitV | Layout::Stacked => direction.is_vertical(),
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

#[cfg(test)]
fn layout_label(layout: Layout) -> &'static str {
    match layout {
        Layout::SplitH => "SplitH",
        Layout::SplitV => "SplitV",
        Layout::Tabbed => "Tabbed",
        Layout::Stacked => "Stacked",
    }
}
