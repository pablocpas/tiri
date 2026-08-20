//! i3-style container tree implementation
//!
//! This module implements the hierarchical container system used by i3wm.
//! Containers form a tree where every node below the workspace is one kind of thing, sway's
//! `sway_container`: it is a window when it holds a tile, and a split when it holds children.
//! Each carries the same state either way — layout (SplitH, SplitV, Tabbed, Stacked), size
//! fractions, floating geometry — which is what lets a window float, be split, or cross a
//! workspace without anything having to wrap it first.
//!
//! Nodes use process-wide keys so moving a node between workspace stores does not change its
//! identity. Each workspace still owns its topology and geometry caches. This is the Rust
//! ownership form of sway moving the same `sway_container` through `container_detach` and
//! `workspace_add_tiling`/`workspace_add_floating`.
//!
//! sway/tree/workspace.c:797-852

use std::collections::hash_map::Entry;
use std::collections::HashMap;
use std::num::NonZeroU64;
use std::ops::{Deref, DerefMut};
use std::rc::Rc;

use smithay::utils::{Logical, Point, Rectangle, Size};

use super::tile::Tile;
use super::{LayoutElement, Options, SizeFrac};
use crate::utils::id::IdCounter;
use crate::utils::transaction::Transaction;
use tiri_config::BlockOutFrom;

mod branch;
mod cleanup;
mod command;
mod debug;
mod detach;
mod floating_region;
mod focus;
mod fractions;
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
mod seat;
mod split;
mod state;
mod swap;
mod tab_bar_model;
mod tree_store;

pub(super) use floating_region::{floating_position_from_logical, scale_floating_position};
use geometry::PendingLayout;
pub(super) use resize::ResizeTarget;

// ============================================================================
// Node identity
// ============================================================================

static NODE_ID_COUNTER: IdCounter = IdCounter::new();

/// Stable identity of a node, including while it moves between workspace stores.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct NodeKey(NonZeroU64);

impl NodeKey {
    pub(super) fn next() -> Self {
        Self(NonZeroU64::new(NODE_ID_COUNTER.next()).expect("node key space exhausted"))
    }
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
#[derive(Debug, Clone)]
pub(in crate::layout) struct ResizeSpace {
    pub min_size: f64,
    /// Settled pixel span of every child along the resized axis.
    pub child_spans: Vec<f64>,
}

/// One tiled resize delta in sway's input coordinate system.
///
/// Fractions are derived inside `resize_child` from the settled pixel spans. Keeping the
/// original pixel delta also avoids converting a fixed request to a fraction and back, where
/// a value such as 150 px can become 150.00000000000003 and make `ceil` reject a resize that
/// lands exactly on the minimum.
#[derive(Debug, Clone, Copy)]
pub(in crate::layout) struct ResizeDelta {
    pub pixels: f64,
}

#[derive(Debug, Clone, Copy, Default, PartialEq)]
pub(super) struct ChildFractions {
    width: f64,
    height: f64,
}

/// The size state sway stores on each `sway_container`, independent of its parent.
///
/// Fractions travel when a node is reparented. The totals remember which available span the
/// last arranged pixel size came from, so resize can snap the fraction back from that exact
/// rounded size before applying its delta.
///
/// sway/include/sway/tree/container.h:127-133
#[derive(Debug, Clone, Copy, Default, PartialEq)]
pub(super) struct NodeSizing {
    fractions: ChildFractions,
    child_total_width: f64,
    child_total_height: f64,
    /// Node box saved on entry to workspace fullscreen (`saved_*` in Sway).
    fullscreen_restore_geometry: Option<Rectangle<f64, Logical>>,
}

impl NodeSizing {
    fn fraction(self, layout: Layout) -> f64 {
        match layout {
            Layout::SplitV => self.fractions.height,
            Layout::SplitH | Layout::Tabbed | Layout::Stacked => self.fractions.width,
        }
    }

    fn set_fraction(&mut self, layout: Layout, fraction: f64) {
        match layout {
            Layout::SplitV => self.fractions.height = fraction,
            Layout::SplitH | Layout::Tabbed | Layout::Stacked => self.fractions.width = fraction,
        }
    }

    fn child_total(self, layout: Layout) -> f64 {
        match layout {
            Layout::SplitH => self.child_total_width,
            Layout::SplitV => self.child_total_height,
            Layout::Tabbed | Layout::Stacked => 0.0,
        }
    }

    fn set_child_total(&mut self, layout: Layout, total: f64) {
        match layout {
            Layout::SplitH => self.child_total_width = total,
            Layout::SplitV => self.child_total_height = total,
            Layout::Tabbed | Layout::Stacked => {}
        }
    }

    pub(super) fn unset_fractions(&mut self) {
        self.fractions = ChildFractions::default();
    }
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
    /// The workspace node. It owns the tiled child list but is not a container.
    ///
    /// sway's `N_WORKSPACE`: a struct of its own, unlike i3 where a workspace is an ordinary
    /// `Con`. Modelling it as a node anyway is what lets one code path answer the questions
    /// both of them answer the same way.
    Workspace(WorkspaceData),
    /// Every other node: sway's `N_CONTAINER`.
    ///
    /// One `sway_container` is a view when `->view` is set and a split when it has children,
    /// and it carries the same state either way. A separate leaf type is what left a window
    /// unable to hold what a container holds — geometry above all, which is why floating a
    /// window used to need a wrapper around it.
    Container(ContainerData<W>),
}

impl<W: LayoutElement> NodeData<W> {
    /// The view this node is, when it is one.
    pub(super) fn as_tile(&self) -> Option<&Tile<W>> {
        match self {
            NodeData::Container(container) => container.tile(),
            NodeData::Workspace(_) => None,
        }
    }

    pub(super) fn as_tile_mut(&mut self) -> Option<&mut Tile<W>> {
        match self {
            NodeData::Container(container) => container.tile_mut(),
            NodeData::Workspace(_) => None,
        }
    }

    /// Whether this node is a window, as opposed to the workspace or a split.
    pub(super) fn is_view(&self) -> bool {
        self.as_tile().is_some()
    }

    /// Whether this node lays out children: a split container, not a view.
    pub(super) fn is_split(&self) -> bool {
        matches!(self, NodeData::Container(container) if !container.is_view())
    }

    pub(super) fn into_tile(self) -> Option<Tile<W>> {
        match self {
            NodeData::Container(container) => container.into_tile(),
            NodeData::Workspace(_) => None,
        }
    }
}

/// Detached subtree used to move container structures across trees.
///
/// One type, the same way the arena has one: a node carries a tile or children, never both.
/// Splitting this in two was what made a window in transit unable to say anything a container
/// can say, so the two halves had to be kept in step by hand at both ends of every move.
#[derive(Debug)]
pub struct DetachedNode<W: LayoutElement> {
    key: NodeKey,
    sizing: NodeSizing,
    layout: Layout,
    /// The view this node *is*, when it is one. Mirrors `ContainerData::tile`.
    tile: Option<Tile<W>>,
    children: Vec<DetachedNode<W>>,
    focus_stack: Vec<NodeKey>,
    user_created: bool,
    prev_split_layout: Option<Layout>,
}

/// Fields shared by the workspace and real containers because both lay out child nodes.
#[derive(Debug)]
pub struct LayoutParentData {
    layout: Layout,
    children: Vec<NodeKey>,
    prev_split_layout: Option<Layout>,
    geometry: Rectangle<f64, Logical>,
}

/// Workspace data stored under its own stable node identity.
#[derive(Debug)]
pub struct WorkspaceData {
    common: LayoutParentData,
}

/// Container data stored under a stable node identity.
#[derive(Debug)]
pub struct ContainerData<W: LayoutElement> {
    common: LayoutParentData,
    /// The view this node *is*, when it is one. sway's `sway_container->view`.
    ///
    /// A node holding a tile never holds children, and vice versa.
    tile: Option<Tile<W>>,
    /// Preserve container even if it has a single child (explicit split).
    user_created: bool,
    /// The fractions and resize reference spans belonging to this node itself.
    sizing: NodeSizing,
    /// The complete geometry state when this node is a floating root.
    ///
    /// Its presence is also the root-membership marker. `FloatingSpace` owns only stacking and
    /// semantic metadata; it never stores a second position, size, or root collection.
    floating_geometry: Option<FloatingGeometry>,
}

/// The single geometry authority for a floating root.
///
/// `target` must be distinct from the last arranged `ContainerData::geometry`: completing an
/// older transaction may replace that cache while a newer compositor target still has to be
/// requested. `resize_base_size` similarly records a client-observed size without overwriting
/// the newer target. They are different meanings, but they live together on the root that owns
/// both rather than being coordinated across `ContainerArena` and `FloatingSpace`.
#[derive(Debug, Clone, Copy, PartialEq)]
struct FloatingGeometry {
    pos: Point<f64, SizeFrac>,
    working_area: Rectangle<f64, Logical>,
    target: Rectangle<f64, Logical>,
    resize_base_size: Size<f64, Logical>,
}

/// Cached layout information for a leaf tile.
#[derive(Debug, Clone)]
pub struct LeafLayoutInfo {
    pub key: NodeKey,
    /// The root of the branch this leaf lives in — the workspace, or a floating group.
    ///
    /// Beside the path because it is half of the address: a path is read from a branch's own
    /// root, which is what sway's `get_tree` publishes as `nodes` and `floating_nodes`.
    pub branch: NodeKey,
    pub path: Vec<usize>,
    /// The box used to place and render the tile after layout decoration is applied.
    pub rect: Rectangle<f64, Logical>,
    /// The pending box belonging to the node itself.
    ///
    /// sway keeps this separate from the rectangle IPC derives after adding a tab or stack
    /// title bar. Tree surgery can change the latter without arranging or changing this one.
    pub node_rect: Rectangle<f64, Logical>,
    pub visible: bool,
    /// This geometry came from `arrange_workspace`'s fullscreen branch.
    ///
    /// A direct `arrange_container` can subsequently give the same fullscreen leaf an
    /// ordinary (or zero) pending box. Keeping the provenance on the cached geometry lets
    /// IPC distinguish those routes without duplicating fullscreen ownership state.
    pub workspace_fullscreen: bool,
}

#[derive(Debug, Clone)]
pub(super) struct InsertParentInfo {
    pub parent_path: Vec<usize>,
    pub insert_idx: usize,
    pub layout: Layout,
}

/// The tiling node chosen by the seat as the insertion context for an immediate unfloat.
///
/// This reference never outlives the workspace mutation that consumes it, so the stable node
/// identity is the complete address. Persistent restore hints use [`InsertParentInfo`] instead:
/// their original parent may be reaped and need rebuilding later.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) struct InactiveTilingReference {
    key: NodeKey,
}

impl InactiveTilingReference {
    fn new(key: NodeKey) -> Self {
        Self { key }
    }

    fn key(self) -> NodeKey {
        self.key
    }
}

/// Container key, child index, available span, child count and rect of a window's container.
pub(super) type ContainerMetrics = (NodeKey, usize, f64, usize, Rectangle<f64, Logical>);

/// Workspace-local ownership of globally identified nodes.
#[derive(Debug)]
struct NodeStore<W: LayoutElement>(HashMap<NodeKey, NodeData<W>>);

impl<W: LayoutElement> NodeStore<W> {
    fn new() -> Self {
        Self(HashMap::new())
    }

    fn insert(&mut self, key: NodeKey, node: NodeData<W>) {
        match self.0.entry(key) {
            Entry::Vacant(entry) => {
                entry.insert(node);
            }
            Entry::Occupied(_) => panic!("a node key cannot belong to the same workspace twice"),
        }
    }

    fn get(&self, key: NodeKey) -> Option<&NodeData<W>> {
        self.0.get(&key)
    }

    fn get_mut(&mut self, key: NodeKey) -> Option<&mut NodeData<W>> {
        self.0.get_mut(&key)
    }

    fn remove(&mut self, key: NodeKey) -> Option<NodeData<W>> {
        self.0.remove(&key)
    }

    fn contains_key(&self, key: NodeKey) -> bool {
        self.0.contains_key(&key)
    }

    fn len(&self) -> usize {
        self.0.len()
    }

    fn keys(&self) -> impl Iterator<Item = NodeKey> + '_ {
        self.0.keys().copied()
    }

    fn iter(&self) -> impl Iterator<Item = (NodeKey, &NodeData<W>)> + '_ {
        self.0.iter().map(|(key, node)| (*key, node))
    }

    fn iter_mut(&mut self) -> impl Iterator<Item = (NodeKey, &mut NodeData<W>)> + '_ {
        self.0.iter_mut().map(|(key, node)| (*key, node))
    }
}

#[derive(Debug)]
struct ParentStore(HashMap<NodeKey, Option<NodeKey>>);

impl ParentStore {
    fn new() -> Self {
        Self(HashMap::new())
    }

    fn insert(&mut self, key: NodeKey, parent: Option<NodeKey>) {
        self.0.insert(key, parent);
    }

    fn get(&self, key: NodeKey) -> Option<&Option<NodeKey>> {
        self.0.get(&key)
    }

    fn get_mut(&mut self, key: NodeKey) -> Option<&mut Option<NodeKey>> {
        self.0.get_mut(&key)
    }

    fn remove(&mut self, key: NodeKey) -> Option<Option<NodeKey>> {
        self.0.remove(&key)
    }
}

/// Stable node arena and topology backing a workspace's shared container tree.
#[derive(Debug)]
pub(super) struct ContainerArena<W: LayoutElement> {
    /// Nodes currently belonging to this workspace.
    nodes: NodeStore<W>,
    /// Parent pointer for each node (None for root)
    parents: ParentStore,
    /// The workspace.
    ///
    /// Always present as a [`NodeData::Workspace`], empty tree included.
    ///
    /// sway keeps `sway_workspace` and `sway_container` as separate structs, but the
    /// workspace carries the same `layout` and `prev_split_layout` and answers the same
    /// questions — it is a container's worth of state under another type, which is why i3,
    /// where a workspace is an ordinary `Con`, gets the same answers out of one code path.
    /// Modelling it as a node here buys that same uniformity: every rule that used to ask
    /// "is the parent the workspace?" was working around its absence, and each of those
    /// workarounds disagreed with sway somewhere.
    root: NodeKey,
    /// The seat's focus: what holds it, and the order everything was last in.
    ///
    /// Behind a type with private fields because the three used to be loose values assigned
    /// from thirty places, of which one also kept the order. Every rule that reads the order
    /// — which tab a switcher shows, which window a descent lands on — was therefore right
    /// for the commands whose authors remembered and wrong for the rest.
    seat: seat::SeatFocus,
    fullscreen_key: Option<NodeKey>,
    /// Cached layout info for leaves
    leaf_layouts: Vec<LeafLayoutInfo>,
    /// Branches whose arrange is waiting for the windows it asked to resize.
    ///
    /// One entry per branch, because a branch's windows share space with each other and with
    /// nothing outside it. See [`PendingLayout`].
    pending_layouts: Vec<PendingLayout>,
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
    /// Generation counter for layout invalidation.
    generation: u64,
}

#[derive(Debug, Clone, Copy)]
pub(super) struct PreviewLeafGeometry {
    pub rect: Rectangle<f64, Logical>,
    pub tab_bar_offset: f64,
}

// ============================================================================
// Workspace and container data implementation
// ============================================================================

impl LayoutParentData {
    fn new(layout: Layout) -> Self {
        Self {
            layout,
            children: Vec::new(),
            prev_split_layout: None,
            geometry: Rectangle::from_size(Size::from((0.0, 0.0))),
        }
    }

    /// Get container layout
    pub(super) fn layout(&self) -> Layout {
        self.layout
    }

    /// Set container layout
    pub(super) fn set_layout(&mut self, layout: Layout) {
        self.layout = layout;
    }

    /// Apply `cmd_layout`'s history rule rather than a structural orientation change.
    pub(super) fn set_layout_from_command(&mut self, layout: Layout) {
        if self.layout != layout && matches!(self.layout, Layout::SplitH | Layout::SplitV) {
            self.prev_split_layout = Some(self.layout);
        }
        self.layout = layout;
    }

    /// `workspace_split` is the one split path that writes workspace layout history, and it
    /// does so unconditionally when the workspace is empty.
    pub(super) fn set_layout_from_empty_workspace_split(&mut self, layout: Layout) {
        self.prev_split_layout = Some(self.layout);
        self.layout = layout;
    }

    pub(super) fn prev_split_layout(&self) -> Option<Layout> {
        self.prev_split_layout
    }

    /// Get children keys
    pub(super) fn children(&self) -> &[NodeKey] {
        &self.children
    }

    /// Number of children
    pub(super) fn child_count(&self) -> usize {
        self.children.len()
    }

    /// Add a child node by key
    pub(super) fn add_child(&mut self, node_key: NodeKey) {
        let idx = self.children.len();
        self.insert_child(idx, node_key);
    }

    /// Remove a child the way sway's `container_detach` removes one from its siblings.
    ///
    /// The surviving fractions stay raw. The following arrange normalizes them, and doing it
    /// any earlier loses the settled-pixel bias left by resize once one child disappears.
    ///
    /// sway/tree/container.c:1503-1521
    pub(super) fn remove_child(&mut self, idx: usize) -> Option<NodeKey> {
        if idx >= self.children.len() {
            return None;
        }

        Some(self.children.remove(idx))
    }

    /// Get child key at index
    pub(super) fn child_key(&self, idx: usize) -> Option<NodeKey> {
        self.children.get(idx).copied()
    }

    pub(super) fn insert_child(&mut self, idx: usize, node_key: NodeKey) {
        let idx = idx.min(self.children.len());
        self.children.insert(idx, node_key);
    }

    /// Exchange two children. Their fractions move with the nodes themselves.
    ///
    /// A swap is the one move where both nodes keep what they had: sway's neighbour-swap
    /// branch trades the containers' positions and their fractions together, so neither
    /// arrives anywhere needing a share worked out.
    pub(super) fn swap_child_slots(&mut self, a: usize, b: usize) {
        self.children.swap(a, b);
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

impl WorkspaceData {
    fn new(layout: Layout) -> Self {
        Self {
            common: LayoutParentData::new(layout),
        }
    }
}

impl Deref for WorkspaceData {
    type Target = LayoutParentData;

    fn deref(&self) -> &Self::Target {
        &self.common
    }
}

impl DerefMut for WorkspaceData {
    fn deref_mut(&mut self) -> &mut Self::Target {
        &mut self.common
    }
}

impl<W: LayoutElement> ContainerData<W> {
    /// Create a new split container with given layout.
    pub(super) fn new(layout: Layout) -> Self {
        Self {
            common: LayoutParentData::new(layout),
            tile: None,
            user_created: false,
            sizing: NodeSizing::default(),
            floating_geometry: None,
        }
    }

    /// Create the node a window *is*.
    pub(super) fn new_view(tile: Tile<W>) -> Self {
        Self {
            common: LayoutParentData::new(Layout::SplitH),
            tile: Some(tile),
            user_created: false,
            sizing: NodeSizing::default(),
            floating_geometry: None,
        }
    }

    pub(super) fn tile(&self) -> Option<&Tile<W>> {
        self.tile.as_ref()
    }

    pub(super) fn tile_mut(&mut self) -> Option<&mut Tile<W>> {
        self.tile.as_mut()
    }

    pub(super) fn is_view(&self) -> bool {
        self.tile.is_some()
    }

    pub(super) fn into_tile(self) -> Option<Tile<W>> {
        self.tile
    }

    /// Where this node's fractions live: on the tile when it is a view.
    pub(super) fn sizing(&self) -> &NodeSizing {
        match &self.tile {
            Some(tile) => tile.node_sizing(),
            None => &self.sizing,
        }
    }

    pub(super) fn sizing_mut(&mut self) -> &mut NodeSizing {
        match &mut self.tile {
            Some(tile) => tile.node_sizing_mut(),
            None => &mut self.sizing,
        }
    }

    pub(super) fn set_layout_explicit(&mut self, layout: Layout) {
        self.set_layout(layout);
        self.user_created = true;
    }

    pub(super) fn set_layout_explicit_from_command(&mut self, layout: Layout) {
        self.set_layout_from_command(layout);
        self.user_created = true;
    }

    /// Whether this container is one the user asked for, rather than scaffolding.
    pub(super) fn is_user_container(&self) -> bool {
        self.user_created
    }

    pub(super) fn mark_user_created(&mut self) {
        self.user_created = true;
    }
}

impl<W: LayoutElement> Deref for ContainerData<W> {
    type Target = LayoutParentData;

    fn deref(&self) -> &Self::Target {
        &self.common
    }
}

impl<W: LayoutElement> DerefMut for ContainerData<W> {
    fn deref_mut(&mut self) -> &mut Self::Target {
        &mut self.common
    }
}

// ============================================================================
// Detached subtree helpers
// ============================================================================

impl<W: LayoutElement> DetachedNode<W> {
    /// The node a window is, keeping the key it already had.
    pub(super) fn new_view(tile: Tile<W>) -> Self {
        Self {
            key: tile.node_key(),
            sizing: NodeSizing::default(),
            layout: Layout::SplitH,
            tile: Some(tile),
            children: Vec::new(),
            focus_stack: Vec::new(),
            user_created: false,
            prev_split_layout: None,
        }
    }

    pub(super) fn new_container(layout: Layout, children: Vec<DetachedNode<W>>) -> Self {
        let focus_stack = children.iter().map(|child| child.key).collect();
        Self {
            key: NodeKey::next(),
            sizing: NodeSizing::default(),
            layout,
            tile: None,
            children,
            focus_stack,
            user_created: false,
            prev_split_layout: None,
        }
    }

    /// Where this node's fractions live: on the tile when it is a view, as in the arena.
    pub(super) fn unset_root_fractions(&mut self) {
        match &mut self.tile {
            Some(tile) => tile.unset_node_fractions(),
            None => self.sizing.unset_fractions(),
        }
    }

    pub(super) fn tiles(&self) -> Vec<&Tile<W>> {
        let mut tiles = Vec::new();
        self.collect_tiles(&mut tiles);
        tiles
    }

    fn collect_tiles<'a>(&'a self, tiles: &mut Vec<&'a Tile<W>>) {
        match &self.tile {
            Some(tile) => tiles.push(tile),
            None => {
                for child in &self.children {
                    child.collect_tiles(tiles);
                }
            }
        }
    }

    pub(super) fn contains_window(&self, window_id: &W::Id) -> bool {
        match &self.tile {
            Some(tile) => tile.window().id() == window_id,
            None => self
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

    fn collect_tiles_owned(self, tiles: &mut Vec<Tile<W>>) {
        match self.tile {
            Some(tile) => tiles.push(tile),
            None => {
                for child in self.children {
                    child.collect_tiles_owned(tiles);
                }
            }
        }
    }
}

// ============================================================================
// ContainerArena Implementation
// ============================================================================

impl<W: LayoutElement> ContainerArena<W> {
    /// Create a new empty container tree
    pub(super) fn new(
        view_size: Size<f64, Logical>,
        working_area: Rectangle<f64, Logical>,
        scale: f64,
        options: Rc<Options>,
    ) -> Self {
        let mut nodes = NodeStore::new();
        let mut parents = ParentStore::new();
        let root = NodeKey::next();
        nodes.insert(
            root,
            NodeData::Workspace(WorkspaceData::new(Layout::SplitH)),
        );
        parents.insert(root, None);
        let mut seat = seat::SeatFocus::default();
        seat.register(root);

        Self {
            nodes,
            parents,
            root,
            seat,
            fullscreen_key: None,
            leaf_layouts: Vec::new(),
            pending_layouts: Vec::new(),
            pending_transaction: None,
            pending_relayout: false,
            view_size,
            working_area,
            scale,
            options,
            generation: 0,
        }
    }
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
