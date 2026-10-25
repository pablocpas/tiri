//! i3-style container tree implementation
//!
//! This module implements the hierarchical container system used by i3wm.
//! Containers form a tree where every node below the workspace is one kind of thing, sway's
//! `sway_container`: it is a window when it holds a tile, and a split when it holds children.
//! Each carries the same state either way — layout (SplitH, SplitV, Tabbed, Stacked), size
//! fractions, floating geometry — which is what lets a window float, be split, or cross a
//! workspace without anything having to wrap it first.
//!
//! One tree owns every workspace root, floating branch and in-flight transfer in a
//! `Layout`. A container crossing a workspace or branch is reparented in place, preserving its
//! key, marks, sizing and seat order just as sway's `container_detach` does.
//!
//! Operations scoped to one workspace take its `NodeKey`; tree-wide walks say so in their
//! names. This keeps a local command from silently reading another root while still giving
//! transactions and lookup one authoritative store.
//!
//! sway/tree/workspace.c:797-852

use std::collections::hash_map::Entry;
use std::collections::{HashMap, HashSet};
use std::num::NonZeroU64;
use std::ops::{Deref, DerefMut};
use std::rc::Rc;

use smithay::utils::{Logical, Point, Rectangle, Size};
use tiri_config::BlockOutFrom;

use super::tile::Tile;
use super::workspace::{WorkspaceProperties, WorkspaceRuntime};
use super::{Clock, LayoutElement, Options, SizeFrac};
use crate::utils::id::IdCounter;
use crate::utils::transaction::Transaction;

mod branch;
mod cleanup;
mod command;
mod debug;
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
mod outputs;
mod paths;
mod preview;
mod query;
mod resize;
mod root_children;
mod seat;
mod split;
mod state;
mod sticky;
mod swap;
mod tab_bar_model;
mod transfer;
mod tree_store;

pub use outputs::OutputKey;
use outputs::Outputs;

pub use command::LayoutCommand;
pub(super) use command::WorkspaceCommandTarget;
pub(super) use floating_region::{floating_position_from_logical, scale_floating_position};
pub(in crate::layout) use focus::ActiveChildren;
use geometry::PendingLayout;
pub(super) use resize::{InteractiveResizeState, ResizeTarget};

// ============================================================================
// Node identity
// ============================================================================

static NODE_ID_COUNTER: IdCounter = IdCounter::new();

/// Stable identity of a node, including while it moves between roots.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct NodeKey(NonZeroU64);

impl NodeKey {
    pub(in crate::layout) fn get(self) -> u64 {
        self.0.get()
    }

    pub(super) fn next() -> Self {
        Self(NonZeroU64::new(NODE_ID_COUNTER.next()).expect("node key space exhausted"))
    }
}

// ============================================================================
// Container Types and Enums
// ============================================================================

/// How a container arranges its children: i3's `layout`.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub enum LayoutMode {
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

/// Which root collection of a workspace an operation or focus state addresses.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum LayoutLayer {
    Tiled,
    Floating,
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
    fn fraction(self, layout: LayoutMode) -> f64 {
        match layout {
            LayoutMode::SplitV => self.fractions.height,
            LayoutMode::SplitH | LayoutMode::Tabbed | LayoutMode::Stacked => self.fractions.width,
        }
    }

    fn set_fraction(&mut self, layout: LayoutMode, fraction: f64) {
        match layout {
            LayoutMode::SplitV => self.fractions.height = fraction,
            LayoutMode::SplitH | LayoutMode::Tabbed | LayoutMode::Stacked => {
                self.fractions.width = fraction
            }
        }
    }

    fn child_total(self, layout: LayoutMode) -> f64 {
        match layout {
            LayoutMode::SplitH => self.child_total_width,
            LayoutMode::SplitV => self.child_total_height,
            LayoutMode::Tabbed | LayoutMode::Stacked => 0.0,
        }
    }

    fn set_child_total(&mut self, layout: LayoutMode, total: f64) {
        match layout {
            LayoutMode::SplitH => self.child_total_width = total,
            LayoutMode::SplitV => self.child_total_height = total,
            LayoutMode::Tabbed | LayoutMode::Stacked => {}
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
    pub layout: LayoutMode,
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
    Workspace(WorkspaceData<W>),
    /// Every other node: sway's `N_CONTAINER`.
    ///
    /// One `sway_container` is a view when `->view` is set and a split when it has children,
    /// and it carries the same state either way. One type is what lets a window hold
    /// everything a container holds — floating geometry above all — so floating a window
    /// needs no wrapper around it.
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

/// A newly mapped view that has not entered the container tree yet.
///
/// This is the only value-form boundary. Once materialized, a view remains a node in the one
/// store until final unmap; workspace moves, floating transitions, scratchpad and interactive
/// moves carry its `NodeKey` and never turn it back into this type.
#[derive(Debug)]
pub struct NewView<W: LayoutElement> {
    key: NodeKey,
    sizing: NodeSizing,
    marks: Vec<String>,
    tile: Tile<W>,
}

/// A view entering a branch, either for the first time or from an in-flight tree transfer.
///
/// New maps cross the tree boundary as [`NewView`]. Every internal transfer keeps the
/// materialized node in the store and carries only its stable key; final unmap destroys it.
#[must_use = "an incoming view must be inserted or returned to its caller"]
pub(super) enum IncomingView<W: LayoutElement> {
    /// A view built outside the tree, which has never had a node in it.
    ///
    /// Boxed because the other variant is a key: a tile beside a `NodeKey` makes every
    /// `Result` carrying this enum as large as a tile, and these are returned by value from
    /// the insertion paths.
    New(Box<NewView<W>>),
    Transferred(NodeKey),
}

impl<W: LayoutElement> std::fmt::Debug for IncomingView<W> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::New(_) => f.write_str("IncomingView::New(..)"),
            Self::Transferred(key) => f
                .debug_tuple("IncomingView::Transferred")
                .field(key)
                .finish(),
        }
    }
}

impl<W: LayoutElement> IncomingView<W> {
    #[cfg(test)]
    pub(super) fn key(&self) -> NodeKey {
        match self {
            Self::New(view) => view.key,
            Self::Transferred(key) => *key,
        }
    }
}

impl<W: LayoutElement> From<NewView<W>> for IncomingView<W> {
    fn from(view: NewView<W>) -> Self {
        Self::New(Box::new(view))
    }
}

/// Fields shared by the workspace and real containers because both lay out child nodes.
#[derive(Debug)]
pub struct LayoutParentData {
    layout: LayoutMode,
    children: Vec<NodeKey>,
    prev_split_layout: Option<LayoutMode>,
    geometry: Rectangle<f64, Logical>,
}

/// Workspace data stored under its own stable node identity.
#[derive(Debug)]
pub struct WorkspaceData<W: LayoutElement> {
    common: LayoutParentData,
    /// Floating roots in render order, topmost first.
    ///
    /// A workspace owns both of its root lists. The tiled roots are `common.children`; the
    /// floating roots live here because their stack order and geometry have different
    /// semantics, while still belonging to the same workspace node and the same tree.
    floating_roots: Vec<FloatingRoot>,
    /// The one container currently owning workspace fullscreen.
    fullscreen_key: Option<NodeKey>,
    /// Another arrange was requested while this workspace still had a pending layout.
    pending_relayout: bool,
    /// The box this workspace is laid out in, and the config it is laid out with.
    ///
    /// On the workspace node rather than beside the tree because they are the workspace's and
    /// not the tree's. A workspace left without an output keeps the last box it had, and a
    /// workspace can override the layout config its monitor handed down, so "the tree's view
    /// size" is only an answer while a tree holds exactly one workspace. sway keeps the same
    /// split: the box is `sway_output`'s and `arrange_output` walks it down.
    view_size: Size<f64, Logical>,
    working_area: Rectangle<f64, Logical>,
    options: Rc<Options>,
    /// What the workspace is: its name, its outputs, its own config.
    pub(super) properties: WorkspaceProperties,
    /// The output the workspace is on, if any; sway's `sway_workspace->output`.
    ///
    /// Written only by [`outputs`], together with that output's list.
    output: Option<OutputKey>,
    /// What the workspace is doing right now: animations, a resize in progress, its overview
    /// shadow and background.
    pub(super) runtime: WorkspaceRuntime<W>,
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
    /// sway's `is_sticky`: set by `sticky enable` on any container, but only in effect while
    /// the container is a floating root. Read it through `ContainerTree::is_sticky`.
    sticky: bool,
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
/// Entries live in [`ContainerTree::floating_roots`] in top-to-bottom order. Keeping the key,
/// stable reinsertion id, semantic provenance and complete geometry in one record means a root
/// cannot exist in the tree without its stack slot, or vice versa.
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
/// both rather than being coordinated across the tree and `FloatingRuntime`.
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
    pub layout: LayoutMode,
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

/// One materialized node and its current structural owner.
#[derive(Debug)]
struct NodeRecord<W: LayoutElement> {
    parent: Option<NodeKey>,
    data: NodeData<W>,
}

/// LayoutMode-wide ownership of globally identified nodes.
#[derive(Debug)]
struct NodeStore<W: LayoutElement>(HashMap<NodeKey, NodeRecord<W>>);

impl<W: LayoutElement> NodeStore<W> {
    fn new() -> Self {
        Self(HashMap::new())
    }

    fn insert(&mut self, key: NodeKey, data: NodeData<W>, parent: Option<NodeKey>) {
        match self.0.entry(key) {
            Entry::Vacant(entry) => {
                entry.insert(NodeRecord { parent, data });
            }
            Entry::Occupied(_) => panic!("a node key can only have one materialized record"),
        }
    }

    fn get(&self, key: NodeKey) -> Option<&NodeData<W>> {
        self.0.get(&key).map(|record| &record.data)
    }

    fn get_mut(&mut self, key: NodeKey) -> Option<&mut NodeData<W>> {
        self.0.get_mut(&key).map(|record| &mut record.data)
    }

    fn remove(&mut self, key: NodeKey) -> Option<NodeData<W>> {
        self.0.remove(&key).map(|record| record.data)
    }

    fn parent(&self, key: NodeKey) -> Option<NodeKey> {
        self.0.get(&key).and_then(|record| record.parent)
    }

    fn set_parent(&mut self, key: NodeKey, parent: Option<NodeKey>) -> bool {
        let Some(record) = self.0.get_mut(&key) else {
            return false;
        };
        record.parent = parent;
        true
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
        self.0.iter().map(|(key, record)| (*key, &record.data))
    }

    fn iter_mut(&mut self) -> impl Iterator<Item = (NodeKey, &mut NodeData<W>)> + '_ {
        self.0
            .iter_mut()
            .map(|(key, record)| (*key, &mut record.data))
    }
}

/// The layout's one container tree: all workspaces, floating branches and in-flight transfers.
#[derive(Debug)]
pub struct ContainerTree<W: LayoutElement> {
    /// Every materialized node in the layout.
    nodes: NodeStore<W>,
    /// Unique window identity to its stable tree node.
    window_nodes: HashMap<W::Id, NodeKey>,
    /// The workspaces this tree holds, in creation order.
    ///
    /// A list rather than one key because the walks that mean *every* node this tree holds
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
    /// Modelling it as a node here buys that same uniformity: no rule has to special-case
    /// "the parent is the workspace", and every such special case is a place to disagree
    /// with sway.
    workspaces: Vec<NodeKey>,
    /// The outputs and the workspaces on each, in order.
    outputs: Outputs,
    /// Roots temporarily owned by the layout but attached to no workspace.
    ///
    /// Interactive moves live here between pointer events. They stay in the one node store,
    /// so identity, marks and window lookup do not acquire a second authority while the view
    /// is being dragged.
    transfer_roots: HashSet<NodeKey>,
    next_floating_root_id: u64,
    /// The seat's focus: what holds it, and the order everything was last in.
    ///
    /// Behind a type with private fields so that focusing something and updating the order
    /// cannot be done separately: every rule that reads the order — which tab a switcher
    /// shows, which window a descent lands on — depends on the two agreeing.
    seat: seat::SeatFocus,
    /// Cached presentation by stable leaf identity.
    leaf_layouts: HashMap<NodeKey, LeafLayoutInfo>,
    /// Branches whose arrange is waiting for the windows it asked to resize.
    ///
    /// One entry per branch, because a branch's windows share space with each other and with
    /// nothing outside it. See [`PendingLayout`].
    pending_layouts: Vec<PendingLayout>,
    /// Optional transaction to use for the next atomic layout.
    pending_transaction: Option<Transaction>,
    /// Generation counter for layout invalidation.
    generation: u64,
    /// Animation clock.
    pub(super) clock: Clock,
    /// Derived render data for the tree: tab bars already drawn, overview textures, scratch.
    ///
    /// The one thing this type holds that is not the model. It lives here because its entries
    /// are keyed by node and must go when their node does; the drawing itself is in
    /// `container_tree::render`.
    pub(super) render: super::container_tree::ContainerRenderState,
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
    fn new(layout: LayoutMode) -> Self {
        Self {
            layout,
            children: Vec::new(),
            prev_split_layout: None,
            geometry: Rectangle::from_size(Size::from((0.0, 0.0))),
        }
    }

    /// Get container layout
    pub(super) fn layout(&self) -> LayoutMode {
        self.layout
    }

    /// Set container layout
    pub(super) fn set_layout(&mut self, layout: LayoutMode) {
        self.layout = layout;
    }

    /// Apply `cmd_layout`'s history rule rather than a structural orientation change.
    pub(super) fn set_layout_from_command(&mut self, layout: LayoutMode) {
        if self.layout != layout && matches!(self.layout, LayoutMode::SplitH | LayoutMode::SplitV) {
            self.prev_split_layout = Some(self.layout);
        }
        self.layout = layout;
    }

    /// `workspace_split` is the one split path that writes workspace layout history, and it
    /// does so unconditionally when the workspace is empty.
    pub(super) fn set_layout_from_empty_workspace_split(&mut self, layout: LayoutMode) {
        self.prev_split_layout = Some(self.layout);
        self.layout = layout;
    }

    pub(super) fn prev_split_layout(&self) -> Option<LayoutMode> {
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

impl<W: LayoutElement> WorkspaceData<W> {
    fn new(
        layout: LayoutMode,
        view_size: Size<f64, Logical>,
        working_area: Rectangle<f64, Logical>,
        options: Rc<Options>,
        properties: WorkspaceProperties,
    ) -> Self {
        Self {
            common: LayoutParentData::new(layout),
            floating_roots: Vec::new(),
            fullscreen_key: None,
            pending_relayout: false,
            output: None,
            runtime: WorkspaceRuntime::new(view_size, &options),
            view_size,
            working_area,
            options,
            properties,
        }
    }
}

impl<W: LayoutElement> Deref for WorkspaceData<W> {
    type Target = LayoutParentData;

    fn deref(&self) -> &Self::Target {
        &self.common
    }
}

impl<W: LayoutElement> DerefMut for WorkspaceData<W> {
    fn deref_mut(&mut self) -> &mut Self::Target {
        &mut self.common
    }
}

impl<W: LayoutElement> ContainerData<W> {
    /// Create a new split container with given layout.
    pub(super) fn new(layout: LayoutMode) -> Self {
        Self {
            common: LayoutParentData::new(layout),
            marks: Vec::new(),
            tile: None,
            user_created: false,
            sizing: NodeSizing::default(),
            sticky: false,
        }
    }

    /// Materialize a new window as a node, returning its reserved identity and payload.
    pub(super) fn from_new_view(view: NewView<W>) -> (NodeKey, Self) {
        let NewView {
            key,
            sizing,
            marks,
            tile,
        } = view;
        let node = Self {
            common: LayoutParentData::new(LayoutMode::SplitH),
            marks,
            tile: Some(tile),
            user_created: false,
            sizing,
            sticky: false,
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

    pub(super) fn set_layout_explicit(&mut self, layout: LayoutMode) {
        self.set_layout(layout);
        self.user_created = true;
    }

    pub(super) fn set_layout_explicit_from_command(&mut self, layout: LayoutMode) {
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

    pub(super) fn sticky_flag(&self) -> bool {
        self.sticky
    }

    pub(super) fn set_sticky_flag(&mut self, sticky: bool) {
        self.sticky = sticky;
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

impl<W: LayoutElement> NewView<W> {
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
    /// Deliberately not a `Deref`: this is the map input, while `Tile` is only its
    /// visual/protocol payload.
    pub(super) fn tile(&self) -> &Tile<W> {
        &self.tile
    }

    pub(super) fn tile_mut(&mut self) -> &mut Tile<W> {
        &mut self.tile
    }
}

// ============================================================================
// ContainerTree implementation
// ============================================================================

impl<W: LayoutElement> ContainerTree<W> {
    /// A new tree with no workspaces in it.
    ///
    /// A workspace root only appears through [`Self::add_workspace`], which is what every
    /// `Workspace` calls when it is built. An tree that fabricated one of its own would hand
    /// out a root that no workspace owns.
    pub fn new(clock: Clock) -> Self {
        let nodes = NodeStore::new();
        let seat = seat::SeatFocus::default();

        Self {
            nodes,
            window_nodes: HashMap::new(),
            workspaces: Vec::new(),
            outputs: Outputs::default(),
            transfer_roots: HashSet::new(),
            next_floating_root_id: 1,
            seat,
            leaf_layouts: HashMap::new(),
            pending_layouts: Vec::new(),
            pending_transaction: None,
            generation: 0,
            clock,
            render: super::container_tree::ContainerRenderState::new(),
        }
    }

    /// Add a workspace to this tree and return its node.
    ///
    /// A workspace root is a node with no parent, like the one the tree is created with and
    /// like a floating root before it is registered. Nothing else distinguishes it: the walks
    /// iterate `workspaces`, `workspace_of` finds it by climbing to the parentless ancestor,
    /// and every operation is already told which one it acts on.
    ///
    /// It carries its own box and config because those are the workspace's — a second
    /// workspace on the same output can be transferred, or override the layout its monitor handed
    /// down, and then the two differ while sharing a tree.
    ///
    pub(in crate::layout) fn add_workspace_node(
        &mut self,
        layout: LayoutMode,
        view_size: Size<f64, Logical>,
        working_area: Rectangle<f64, Logical>,
        options: Rc<Options>,
        properties: WorkspaceProperties,
    ) -> NodeKey {
        let key = NodeKey::next();
        self.nodes.insert(
            key,
            NodeData::Workspace(WorkspaceData::new(
                layout,
                view_size,
                working_area,
                options,
                properties,
            )),
            None,
        );
        self.seat.register(key);
        self.workspaces.push(key);
        key
    }

    /// Drop a workspace and everything under it.
    ///
    /// The last one may go too: a tree with no workspaces is the state a fresh one starts
    /// in, and nothing reads a root it was not given.
    pub(in crate::layout) fn remove_workspace(&mut self, workspace: NodeKey) -> bool {
        let Some(idx) = self.workspaces.iter().position(|key| *key == workspace) else {
            return false;
        };

        self.detach_workspace_from_its_output(workspace);
        for root in self.floating_roots_in(workspace) {
            assert!(self.remove_floating_branch(root));
        }
        self.remove_node_recursive(workspace);
        self.workspaces.remove(idx);
        // The seat forgets every node that went, and nothing is focused in its place: which
        // workspace takes focus next is the layout's to decide, not this tree's.
        self.prune_focus_order();
        self.drop_stale_leaf_layouts();
        true
    }

    /// The floating roots belonging to one workspace.
    pub(in crate::layout) fn floating_roots_in(&self, workspace: NodeKey) -> Vec<NodeKey> {
        self.floating_roots(workspace).collect()
    }

    /// Check every workspace this tree holds, and then the tree itself.
    ///
    /// Reachability belongs here rather than in [`Self::verify_invariants`]: "every node is
    /// under a root" is a claim about the tree, and a per-workspace check can only see one
    /// root's worth of nodes. So does where the seat's focused leaf points, and what the layout
    /// caches describe — all three are the tree's, and all three read a second workspace's
    /// perfectly good state as garbage when asked from the first.
    pub(in crate::layout) fn verify_all_invariants(&self) {
        for workspace in &self.workspaces {
            self.verify_workspace_invariants(*workspace);
        }
        self.verify_outputs();
        self.verify_every_node_is_under_a_root();
    }
}

// ============================================================================
// Additional helper implementations
// ============================================================================

impl LayoutMode {
    /// Whether children of a container with this layout are arranged left to right. Tabs
    /// count as horizontal and stacks as vertical: their titles run that way, and so does
    /// every question about them that has an axis in it.
    pub fn is_horizontal(self) -> bool {
        matches!(self, LayoutMode::SplitH | LayoutMode::Tabbed)
    }

    /// Whether children of a container with this layout are arranged along `direction`'s axis,
    /// so that moving or focusing in that direction steps between siblings.
    pub fn is_parallel_to(self, direction: Direction) -> bool {
        self.is_horizontal() == direction.is_horizontal()
    }

    /// Whether two layouts arrange their children along the same axis.
    pub fn is_parallel_to_layout(self, other: LayoutMode) -> bool {
        self.is_horizontal() == other.is_horizontal()
    }

    /// Next layout in sway's `layout toggle all` cycle:
    /// SplitH -> SplitV -> Stacked -> Tabbed -> SplitH.
    pub fn next_in_cycle(self) -> LayoutMode {
        match self {
            LayoutMode::SplitH => LayoutMode::SplitV,
            LayoutMode::SplitV => LayoutMode::Stacked,
            LayoutMode::Stacked => LayoutMode::Tabbed,
            LayoutMode::Tabbed => LayoutMode::SplitH,
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
    pub fn split_layout(self) -> LayoutMode {
        if self.is_horizontal() {
            LayoutMode::SplitH
        } else {
            LayoutMode::SplitV
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
/// what the unset one held before. That average is why a container emptied by a move lands
/// on exactly `1/n`, and why a share must not be decided at the moment of the mutation but
/// left unset to be filled in here.
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
fn layout_label(layout: LayoutMode) -> &'static str {
    match layout {
        LayoutMode::SplitH => "SplitH",
        LayoutMode::SplitV => "SplitV",
        LayoutMode::Tabbed => "Tabbed",
        LayoutMode::Stacked => "Stacked",
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
