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
//! # Every operation names its workspace
//!
//! An arena holds one workspace today, and its root is that workspace's node. That is not
//! written into the operations: anything that acts on a workspace takes it as a `workspace:
//! NodeKey` first argument rather than reading a field, and `ContainerTree` — the per-workspace
//! handle — is what supplies it.
//!
//! The point is not ceremony. `self.root` is a correct answer to "which workspace?" only
//! because there is exactly one, and an assumption that is right by arithmetic reads exactly
//! like one that is right by design. Naming the workspace makes the two distinguishable, and
//! leaves the arena free to hold several — which is what a single tree, with the workspaces as
//! its roots, needs.
//!
//! So the arena keeps a list of workspaces rather than a root, and the walks that mean *every*
//! node it holds — `all_windows`, `all_tiles`, `dfs_leaf_keys`, `window_count`, `is_empty` —
//! iterate it. A walk that starts from one root would be silently incomplete the moment a
//! second workspace arrived, and that is the failure that does not announce itself: a window
//! that exists and is not found.
//!
//! One thing still assumes the single workspace, and deliberately says so in one place:
//! [`ContainerArena::workspace_root`]. Every caller of it is a place that has not yet been
//! told which workspace it means.
//!
//! sway/tree/workspace.c:797-852

use std::collections::hash_map::Entry;
use std::collections::HashMap;
use std::num::NonZeroU64;
use std::ops::{Deref, DerefMut};
use std::rc::Rc;

use smithay::utils::{Logical, Point, Rectangle, Size};
use tiri_config::BlockOutFrom;

use super::tile::Tile;
use super::{LayoutElement, Options, SizeFrac};
use crate::utils::id::IdCounter;
use crate::utils::transaction::Transaction;

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
mod marks;
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
pub(super) use resize::{InteractiveResizeState, ResizeTarget};

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
    /// This tab is its container's selected child.
    ///
    /// On its own that is i3's `focused_inactive`: the tab the container would come back
    /// to. It only reads as `focused` when the seat's focus is actually inside it, which
    /// is what `holds_focus` says.
    pub is_focused: bool,
    /// The focused leaf sits somewhere inside this tab's subtree.
    pub holds_focus: bool,
    pub is_urgent: bool,
    pub block_out_from: Option<BlockOutFrom>,
}

#[derive(Debug, Clone)]
pub struct TabBarInfo {
    /// The container this bar belongs to.
    pub key: NodeKey,
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
}

/// A view node while no workspace arena owns it.
///
/// `Tile` is only the visual/protocol object. Identity, marks and sizing belong to the sway
/// container node and therefore stay beside it while the view crosses workspaces, sits in the
/// scratchpad or follows an interactive move outside every tree.
#[derive(Debug)]
pub struct DetachedView<W: LayoutElement> {
    key: NodeKey,
    sizing: NodeSizing,
    marks: Vec<String>,
    tile: Tile<W>,
}

/// A split node while no workspace arena owns it.
#[derive(Debug)]
pub struct DetachedSplit<W: LayoutElement> {
    key: NodeKey,
    sizing: NodeSizing,
    layout: Layout,
    marks: Vec<String>,
    children: Vec<DetachedNode<W>>,
    focus_stack: Vec<NodeKey>,
    user_created: bool,
    prev_split_layout: Option<Layout>,
}

/// Detached subtree used to move container structures across trees.
///
/// The enum encodes the same invariant as sway's `sway_container`: a node is a view or it is a
/// split. A detached leaf is the exact same [`DetachedView`] used by single-window moves, so
/// removing one window can no longer degrade the node into a bare tile.
#[derive(Debug)]
pub enum DetachedNode<W: LayoutElement> {
    View(Box<DetachedView<W>>),
    Split(DetachedSplit<W>),
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
    /// The box this workspace is laid out in, and the config it is laid out with.
    ///
    /// On the workspace node rather than beside the tree because they are the workspace's and
    /// not the tree's. A workspace parked without an output keeps the last box it had, and a
    /// workspace can override the layout config its monitor handed down, so "the arena's view
    /// size" is only an answer while an arena holds exactly one workspace. sway keeps the same
    /// split: the box is `sway_output`'s and `arrange_output` walks it down.
    view_size: Size<f64, Logical>,
    working_area: Rectangle<f64, Logical>,
    scale: f64,
    options: Rc<Options>,
}

/// Container data stored under a stable node identity.
#[derive(Debug)]
pub struct ContainerData<W: LayoutElement> {
    common: LayoutParentData,
    /// i3/sway marks name the container node, whether it is a view or a split.
    marks: Vec<String>,
    /// The view this node *is*, when it is one. sway's `sway_container->view`.
    ///
    /// A node holding a tile never holds children, and vice versa.
    tile: Option<Tile<W>>,
    /// Preserve container even if it has a single child (explicit split).
    user_created: bool,
    /// The fractions and resize reference spans belonging to this node itself.
    sizing: NodeSizing,
}

/// Semantic provenance of one entry in the workspace's floating stack.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum FloatingRootKind {
    ImplicitWindowGroup,
    FloatedContainer,
    WorkspaceWrapper,
}

impl FloatingRootKind {
    pub(super) fn is_workspace_wrapper(self) -> bool {
        self == Self::WorkspaceWrapper
    }
}

/// The single authority for one floating branch.
///
/// Entries live in [`ContainerArena::floating_roots`] in top-to-bottom order. Keeping the key,
/// stable reinsertion id, semantic provenance and complete geometry in one record means a root
/// cannot exist in the arena without its stack slot, or vice versa.
#[derive(Debug, Clone, Copy, PartialEq)]
struct FloatingRoot {
    id: u64,
    key: NodeKey,
    kind: FloatingRootKind,
    geometry: FloatingGeometry,
}

/// Geometry owned by a [`FloatingRoot`] stack entry.
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

/// Presentation state calculated together for one leaf.
///
/// These values describe one candidate or committed layout. Keeping them beside the content
/// rectangle prevents independently keyed maps from representing only part of a leaf's state.
#[derive(Debug, Clone, Copy, Default)]
struct LeafPresentation {
    tab_bar_offset: f64,
    draw_titlebar: bool,
    in_tabbed_context: bool,
}

impl LeafPresentation {
    fn new(tab_bar_offset: f64, draw_titlebar: bool, in_tabbed_context: bool) -> Self {
        Self {
            tab_bar_offset: tab_bar_offset.max(0.0),
            draw_titlebar,
            in_tabbed_context,
        }
    }
}

/// Cached presentation information for a leaf tile.
///
/// The node's own pending box lives on [`ContainerData`] like it does for every real
/// container. This cache only holds the decorated content box and state needed to present the
/// leaf; keeping those meanings separate prevents tree operations from having to know whether a
/// node currently holds a view or children.
#[derive(Debug, Clone)]
pub struct LeafLayoutInfo {
    pub key: NodeKey,
    /// The box used to place and render the tile after layout decoration is applied.
    pub rect: Rectangle<f64, Logical>,
    presentation: LeafPresentation,
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
    /// The workspaces this arena holds, in creation order. Never empty.
    ///
    /// A list rather than one key because the walks that mean *every* node this arena holds
    /// have to iterate something. A walk that starts from a single root is silently incomplete
    /// the moment a second workspace arrives, and silently incomplete is how a window comes to
    /// exist without being found.
    ///
    /// Each is always a [`NodeData::Workspace`], empty tree included.
    ///
    /// sway keeps `sway_workspace` and `sway_container` as separate structs, but the
    /// workspace carries the same `layout` and `prev_split_layout` and answers the same
    /// questions — it is a container's worth of state under another type, which is why i3,
    /// where a workspace is an ordinary `Con`, gets the same answers out of one code path.
    /// Modelling it as a node here buys that same uniformity: every rule that used to ask
    /// "is the parent the workspace?" was working around its absence, and each of those
    /// workarounds disagreed with sway somewhere.
    workspaces: Vec<NodeKey>,
    /// Floating branches in render order, topmost first.
    ///
    /// This is the only floating-root collection and the only owner of their geometry and
    /// semantic metadata. `FloatingSpace` owns only transient interaction/render state.
    floating_roots: Vec<FloatingRoot>,
    next_floating_root_id: u64,
    /// The seat's focus: what holds it, and the order everything was last in.
    ///
    /// Behind a type with private fields because the three used to be loose values assigned
    /// from thirty places, of which one also kept the order. Every rule that reads the order
    /// — which tab a switcher shows, which window a descent lands on — was therefore right
    /// for the commands whose authors remembered and wrong for the rest.
    seat: seat::SeatFocus,
    fullscreen_key: Option<NodeKey>,
    /// Cached presentation by stable leaf identity.
    leaf_layouts: HashMap<NodeKey, LeafLayoutInfo>,
    /// Branches whose arrange is waiting for the windows it asked to resize.
    ///
    /// One entry per branch, because a branch's windows share space with each other and with
    /// nothing outside it. See [`PendingLayout`].
    pending_layouts: Vec<PendingLayout>,
    /// Optional transaction to use for the next atomic layout.
    pending_transaction: Option<Transaction>,
    /// Whether a new layout is requested while a transaction is pending.
    pending_relayout: bool,
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
    fn new(
        layout: Layout,
        view_size: Size<f64, Logical>,
        working_area: Rectangle<f64, Logical>,
        scale: f64,
        options: Rc<Options>,
    ) -> Self {
        Self {
            common: LayoutParentData::new(layout),
            view_size,
            working_area,
            scale,
            options,
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
            marks: Vec::new(),
            tile: None,
            user_created: false,
            sizing: NodeSizing::default(),
        }
    }

    /// Reattach the node a window is, returning its stable identity with the arena payload.
    pub(super) fn new_view(view: DetachedView<W>) -> (NodeKey, Self) {
        let DetachedView {
            key,
            sizing,
            marks,
            tile,
        } = view;
        let node = Self {
            common: LayoutParentData::new(Layout::SplitH),
            marks,
            tile: Some(tile),
            user_created: false,
            sizing,
        };
        (key, node)
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

    pub(super) fn into_view(self, key: NodeKey) -> Option<DetachedView<W>> {
        Some(DetachedView {
            key,
            sizing: self.sizing,
            marks: self.marks,
            tile: self.tile?,
        })
    }

    pub(super) fn marks(&self) -> &[String] {
        &self.marks
    }

    pub(super) fn has_mark(&self, mark: &str) -> bool {
        self.marks.iter().any(|candidate| candidate == mark)
    }

    pub(super) fn add_mark(&mut self, mark: String) {
        if !self.has_mark(&mark) {
            self.marks.push(mark);
        }
    }

    pub(super) fn remove_mark(&mut self, mark: &str) {
        self.marks.retain(|candidate| candidate != mark);
    }

    pub(super) fn clear_marks(&mut self) {
        self.marks.clear();
    }

    pub(super) fn sizing(&self) -> &NodeSizing {
        &self.sizing
    }

    pub(super) fn sizing_mut(&mut self) -> &mut NodeSizing {
        &mut self.sizing
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
    pub(super) fn unset_root_fractions(&mut self) {
        match self {
            DetachedNode::View(view) => view.sizing.unset_fractions(),
            DetachedNode::Split(split) => split.sizing.unset_fractions(),
        }
    }

    pub(super) fn tiles(&self) -> Vec<&Tile<W>> {
        let mut tiles = Vec::new();
        self.collect_tiles(&mut tiles);
        tiles
    }

    fn collect_tiles<'a>(&'a self, tiles: &mut Vec<&'a Tile<W>>) {
        match self {
            DetachedNode::View(view) => tiles.push(&view.tile),
            DetachedNode::Split(split) => {
                for child in &split.children {
                    child.collect_tiles(tiles);
                }
            }
        }
    }
}

impl<W: LayoutElement> DetachedView<W> {
    pub(super) fn new(tile: Tile<W>) -> Self {
        Self {
            key: NodeKey::next(),
            sizing: NodeSizing::default(),
            marks: Vec::new(),
            tile,
        }
    }

    #[cfg(test)]
    pub(super) fn key(&self) -> NodeKey {
        self.key
    }

    /// The visual object, and only that.
    ///
    /// Deliberately not a `Deref`. A detached node is a node, and the whole reason it is one is
    /// that identity, marks and fractions are not the tile's to answer for; borrowing every
    /// `Tile` method onto this type would put the two vocabularies back in one namespace, where
    /// an inherent method here and a like-named one there differ by which the compiler picks.
    pub(super) fn tile(&self) -> &Tile<W> {
        &self.tile
    }

    pub(super) fn tile_mut(&mut self) -> &mut Tile<W> {
        &mut self.tile
    }

    /// The window this node *is*.
    ///
    /// Forwarded rather than reached through [`Self::tile`] because it is the one thing a
    /// detached node answers in its own right: a view node is a window, in the arena or out of
    /// it. Everything else about the tile is presentation and goes through the accessor.
    pub(super) fn window(&self) -> &W {
        self.tile.window()
    }

    pub(super) fn window_mut(&mut self) -> &mut W {
        self.tile.window_mut()
    }

    pub(super) fn unset_fractions(&mut self) {
        self.sizing.unset_fractions();
    }

    pub(super) fn clear_marks(&mut self) {
        self.marks.clear();
    }

    pub(super) fn has_mark(&self, mark: &str) -> bool {
        self.marks.iter().any(|candidate| candidate == mark)
    }

    pub(super) fn remove_mark(&mut self, mark: &str) {
        self.marks.retain(|candidate| candidate != mark);
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
            NodeData::Workspace(WorkspaceData::new(
                Layout::SplitH,
                view_size,
                working_area,
                scale,
                options,
            )),
        );
        parents.insert(root, None);
        let mut seat = seat::SeatFocus::default();
        seat.register(root);

        Self {
            nodes,
            parents,
            workspaces: vec![root],
            floating_roots: Vec::new(),
            next_floating_root_id: 1,
            seat,
            fullscreen_key: None,
            leaf_layouts: HashMap::new(),
            pending_layouts: Vec::new(),
            pending_transaction: None,
            pending_relayout: false,
            generation: 0,
        }
    }

    /// Add a workspace to this arena and return its node.
    ///
    /// A workspace root is a node with no parent, like the one the arena is created with and
    /// like a floating root before it is registered. Nothing else distinguishes it: the walks
    /// iterate `workspaces`, `workspace_of` finds it by climbing to the parentless ancestor,
    /// and every operation is already told which one it acts on.
    ///
    /// It carries its own box and config because those are the workspace's — a second
    /// workspace on the same output can be parked, or override the layout its monitor handed
    /// down, and then the two differ while sharing an arena.
    ///
    /// Test-only until the arena moves to `Monitor`, which is what will create these. Gating it
    /// says which of the two this commit is: the arena can hold several workspaces and is
    /// checked doing so; nothing in production yet asks it to.
    #[cfg(test)]
    pub(in crate::layout) fn add_workspace(
        &mut self,
        layout: Layout,
        view_size: Size<f64, Logical>,
        working_area: Rectangle<f64, Logical>,
        scale: f64,
        options: Rc<Options>,
    ) -> NodeKey {
        let key = NodeKey::next();
        self.nodes.insert(
            key,
            NodeData::Workspace(WorkspaceData::new(
                layout,
                view_size,
                working_area,
                scale,
                options,
            )),
        );
        self.parents.insert(key, None);
        self.seat.register(key);
        self.workspaces.push(key);
        key
    }

    /// Drop a workspace and everything under it.
    ///
    /// Refuses the last one: an arena without a workspace has no root for anything to be
    /// inserted into, and every caller that would reach that state means to drop the arena.
    #[cfg(test)]
    pub(in crate::layout) fn remove_workspace(&mut self, workspace: NodeKey) -> bool {
        if self.workspaces.len() <= 1 {
            return false;
        }
        let Some(idx) = self.workspaces.iter().position(|key| *key == workspace) else {
            return false;
        };

        for root in self.floating_roots_in(workspace) {
            self.forget_floating_root(root);
        }
        self.remove_node_recursive(workspace);
        self.workspaces.remove(idx);
        self.prune_focus_order();
        self.prune_leaf_layouts(self.workspaces[0]);
        true
    }

    /// The floating roots belonging to one workspace.
    pub(in crate::layout) fn floating_roots_in(&self, workspace: NodeKey) -> Vec<NodeKey> {
        self.floating_roots()
            .filter(|root| self.workspace_of(*root) == Some(workspace))
            .collect()
    }

    /// Check every workspace this arena holds, and then the arena itself.
    ///
    /// Reachability belongs here rather than in [`Self::verify_invariants`]: "every node is
    /// under a root" is a claim about the arena, and a per-workspace check can only see one
    /// root's worth of nodes. So does where the seat's focused leaf points, and what the layout
    /// caches describe — all three are the arena's, and all three read a second workspace's
    /// perfectly good state as garbage when asked from the first.
    pub(in crate::layout) fn verify_all_invariants(&self) {
        for workspace in &self.workspaces {
            self.verify_invariants(*workspace);
        }
        self.verify_every_node_is_under_a_root();
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

    if percents.iter().any(|percent| !percent.is_finite()) {
        // `container_resize_tiled` validates only the command target's historical
        // `child_total_*`. A sibling that has never been arranged on this axis can therefore
        // divide its pending span by zero. In Sway the resulting infinity contaminates the
        // normalization: every non-last child rounds to a zero box and the last child takes
        // the parent's remainder (sway/tree/arrange.c:15-96).
        //
        // Keep that observable result without retaining NaN/inf in the model, where a later
        // renderer or invariant check would turn Sway's arithmetic edge into memory-state
        // corruption. `[0, ..., 1]` is the finite fixed point of the boxes that pass creates.
        let mut collapsed = vec![0.0; count];
        collapsed[count - 1] = 1.0;
        return collapsed;
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

#[cfg(test)]
mod resolved_percent_tests {
    use super::resolved_percents;

    #[test]
    fn a_non_finite_resize_fraction_collapses_into_the_last_child_like_sway() {
        assert_eq!(resolved_percents(&[0.5, f64::INFINITY], 2), [0.0, 1.0]);
        assert_eq!(resolved_percents(&[f64::NAN, 0.5, 0.5], 3), [0.0, 0.0, 1.0],);
    }
}
