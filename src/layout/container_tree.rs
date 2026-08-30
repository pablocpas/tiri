//! The layout's one container store, holding every workspace.
//!
//! sway's workspace holds two lists — `ws->tiling` and `ws->floating` — over one set of
//! containers, and the same container moves between them without being rebuilt. This is that
//! container tree, for every workspace at once. Windows sit in a tree whose internal nodes
//! have a layout mode (SplitH, SplitV, Tabbed, Stacked) and whose leaves are tiles; each
//! workspace's tiled side hangs off its workspace root and each floating group is a root of
//! its own.
//!
//! **Every operation names its workspace.** This type holds no workspace of its own: a
//! `Workspace` is a `NodeKey` into this store plus its presentation state, and it passes that
//! key in. That is what makes moving a window between workspaces a reparent — the node keeps
//! its key, its place in the seat's focus order, its marks — rather than a subtree carried
//! between two arenas and rebuilt with new keys on arrival.
//!
//! Node access is O(1), and node keys remain stable when a subtree changes workspace or
//! output.
//!
//! Render caches derived from the tree live beside it because both branches consume the same
//! projections, and anything that belongs to one workspace is keyed by its root. Transient
//! interaction state does not live here at all: a tiled resize and its closing windows belong
//! to the [`super::workspace::Workspace`] and are handed to the methods that need them, and
//! the floating side's equivalents live in [`super::floating`].

use std::cell::{RefCell, RefMut};
use std::collections::{HashMap, HashSet};
use std::rc::Rc;

use log::warn;
use smithay::backend::renderer::element::Kind;
use smithay::backend::renderer::gles::GlesRenderer;
use smithay::utils::{Logical, Point, Rectangle, Scale, Size};
use tiri_config::utils::MergeWith as _;
use tiri_config::{Border, Color, HideEdgeBorders, PresetSize, TabBar};
use tiri_ipc::{ColumnDisplay, LayoutTreeNode, LayoutTreeRect, SizeChange};

use super::closing_window::{ClosingWindow, ClosingWindowRenderElement};
use super::container::{
    ContainerArena, ContainerMetrics, Direction, IncomingView, InteractiveResizeState, Layout,
    LeafLayoutInfo, NodeKey, ResizeDelta, ResizeReach, ResizeSpace, ResizeTarget,
};
use super::focus_ring::{
    render_container_selection, FocusRingEdges, FocusRingIndicatorEdge, FocusRingRenderElement,
};
use super::monitor::{InsertPosition, SplitIndicator};
use super::tile::{Tile, TileRenderElement};
use super::{
    ConfigureIntent, InteractiveResizeData, LayoutCycleEntry, LayoutElement, Options, ParkedTile,
    ResizeAxis, ResizeHit, ResizeRequest,
};
use crate::animation::{Animation, Clock};
use crate::layout::tab_bar::{
    render_tab_bar, tab_bar_hit_index, tab_bar_state_from_info, TabBarCacheEntry,
    TabBarRenderOutput,
};
use crate::layout::RenderLayer;
use crate::niri_render_elements;
use crate::render_helpers::offscreen::{OffscreenBuffer, OffscreenRenderElement};
use crate::render_helpers::primary_gpu_texture::PrimaryGpuTextureRenderElement;
use crate::render_helpers::renderer::NiriRenderer;
use crate::render_helpers::solid_color::{SolidColorBuffer, SolidColorRenderElement};
use crate::render_helpers::texture::TextureRenderElement;
use crate::render_helpers::xray::XrayPos;
use crate::render_helpers::{RenderCtx, RenderTarget};
use crate::utils::transaction::Transaction;
use crate::utils::{round_logical_in_physical_max1, ResizeEdge};
use crate::window::ResolvedWindowRules;

// sway 1.12's MIN_SANE_W and MIN_SANE_H. Tiled resize refuses the entire operation when
// any participating sibling would cross the relevant pixel floor.
const SWAY_MIN_TILED_WIDTH: f64 = 100.0;
const SWAY_MIN_TILED_HEIGHT: f64 = 60.0;

// ============================================================================
// MAIN STRUCTURES - i3-style container tree implementation
// ============================================================================

/// The layout's container arena, plus the render state derived from it.
///
/// This is deliberately not one workspace and not the tiled side: every workspace's nodes are
/// in `arena`, which also owns the floating-root stack and its geometry. They share tab-bar
/// and render projections and keep stable node identity while crossing sides or workspaces.
/// Tiled-only interaction state — an ongoing resize, the windows closing — belongs to the
/// [`Workspace`] and is handed to the methods that need it.
///
/// Every method takes the workspace it acts on. Nothing here answers "which workspace do you
/// mean"; the caller already knows, and one that did not would be reading another workspace's
/// tree.
///
/// [`Workspace`]: super::workspace::Workspace
///
/// The tree, its render caches, and everything addressed on it.
///
/// Was a wrapper around `ContainerArena` until ago-2026, with 236 of its 252 methods
/// forwarding into it. One tree, one name: this is the arena, and the operations that used
/// to live on the wrapper are the arena's own.
pub type ContainerTree<W> = ContainerArena<W>;

/// Render caches derived from the shared arena.
///
/// Keeping these together makes their ownership explicit without giving either the tiled or
/// floating side a second copy. None of this state is part of the container topology.
#[derive(Debug)]
pub(super) struct ContainerRenderState {
    /// Tab bars already drawn, for both sides of the workspace.
    ///
    /// One cache, because a node key is unique across the arena; two would have to agree on
    /// which side a container is on, and a container changes sides. Entries are dropped when
    /// their container leaves the tree, rather than when a frame does not redraw them: a
    /// fullscreen draws no bars at all and is not a reason to forget every one of them.
    pub(super) tab_bar_cache: RefCell<HashMap<NodeKey, TabBarCacheEntry>>,
    /// Per-workspace overview buffers.
    ///
    /// The overview draws every workspace in one frame, each into its own texture, so a
    /// single offscreen would have every thumbnail overwrite the last.
    pub(super) overview: HashMap<NodeKey, WorkspaceOverview>,
    /// Copy-only state projection reused while mutably visiting tiles.
    pub(super) state_layout_scratch: Vec<LeafFrameInfo>,
    /// Displayed geometry can differ from pending state while a transaction is open.
    pub(super) render_layout_scratch: Vec<LeafFrameInfo>,
    /// Edge projection parallel to `render_layout_scratch`.
    pub(super) render_edges_scratch: Vec<(FocusRingEdges, Option<FocusRingIndicatorEdge>)>,
}

/// One workspace's overview thumbnail: its own texture and the ground under it.
#[derive(Debug)]
pub(super) struct WorkspaceOverview {
    offscreen: OffscreenBuffer,
    pub(super) background: SolidColorBuffer,
}

impl WorkspaceOverview {
    pub(super) fn new(view_size: Size<f64, Logical>, background_color: Color) -> Self {
        Self {
            offscreen: OffscreenBuffer::default(),
            background: SolidColorBuffer::new(view_size, background_color),
        }
    }
}

impl ContainerRenderState {
    pub(super) fn new() -> Self {
        Self {
            tab_bar_cache: RefCell::new(HashMap::new()),
            overview: HashMap::new(),
            state_layout_scratch: Vec::new(),
            render_layout_scratch: Vec::new(),
            render_edges_scratch: Vec::new(),
        }
    }
}

/// A leaf identified for hit-testing: stable node identity and on-screen rect.
type LeafHit = (NodeKey, Rectangle<f64, Logical>);

/// Copyable geometry needed while the tree is borrowed mutably.
///
/// Branch membership is projected from the current tree for committed layouts and from the
/// pending transaction that owns a candidate. Neither is duplicated in `LeafLayoutInfo`.
#[derive(Debug, Clone, Copy)]
pub(super) struct LeafFrameInfo {
    pub(super) key: NodeKey,
    branch: NodeKey,
    pub(super) rect: Rectangle<f64, Logical>,
    pub(super) visible: bool,
}

impl LeafFrameInfo {
    pub(super) fn new(branch: NodeKey, info: &LeafLayoutInfo) -> Self {
        Self {
            key: info.key,
            branch,
            rect: info.rect,
            visible: info.visible,
        }
    }
}

/// Workspace-wide context shared by every tile in an `update_window_state` pass.
struct WindowStateContext<'a, W: LayoutElement> {
    /// The focused leaf. Callers must not run this pass over a stale snapshot, so this is
    /// a plain identity comparison like everywhere else in the module.
    focused_key: Option<NodeKey>,
    workspace_active: bool,
    deactivate_unfocused: bool,
    request_size: bool,
    working_area_size: Size<f64, Logical>,
    options: &'a Options,
    fullscreen_id: Option<&'a W::Id>,
    windowed_fullscreen_id: Option<&'a W::Id>,
    has_fullscreen_container: bool,
    view_size: Size<f64, Logical>,
}

/// The compositor-side fullscreen projection used by rendering and hit testing.
///
/// A container becomes exclusive immediately because no client protocol state represents it.
/// A leaf becomes visual only after its fullscreen configure commits. Windowed fullscreen is
/// considered only when the workspace has no ordinary fullscreen authority.
struct FullscreenRenderState<I> {
    container: Option<NodeKey>,
    fullscreen_id: Option<I>,
    windowed_fullscreen_id: Option<I>,
}

#[derive(Clone, Copy)]
struct FullscreenTileState {
    client_fullscreen: bool,
    visible: bool,
}

impl<I: PartialEq> FullscreenRenderState<I> {
    fn has_client_fullscreen(&self) -> bool {
        self.fullscreen_id.is_some() || self.windowed_fullscreen_id.is_some()
    }

    fn tile_state(
        &self,
        id: &I,
        regular_visible: bool,
        in_fullscreen_container: bool,
    ) -> FullscreenTileState {
        let client_fullscreen = self.fullscreen_id.as_ref().is_some_and(|other| other == id)
            || self
                .windowed_fullscreen_id
                .as_ref()
                .is_some_and(|other| other == id);
        let visible = if self.container.is_some() {
            in_fullscreen_container && regular_visible
        } else if self.has_client_fullscreen() {
            client_fullscreen
        } else {
            regular_visible
        };
        FullscreenTileState {
            client_fullscreen,
            visible,
        }
    }
}

niri_render_elements! {
    ContainerTreeRenderElement<R> => {
        Tile = TileRenderElement<R>,
        TabBar = PrimaryGpuTextureRenderElement,
        ClosingWindow = ClosingWindowRenderElement,
        ContainerSelection = FocusRingRenderElement,
        SolidColor = SolidColorRenderElement,
        Offscreen = OffscreenRenderElement,
    }
}

/// Parked top-level tiling subtree.
///
/// This is the internal unit moved across workspaces and monitors in the i3-style tree.
#[derive(Debug)]
#[must_use = "a parked subtree must be reattached to the layout tree"]
pub struct RootTilingSubtree {
    /// Parked root that preserves the materialized container structure in the shared arena.
    subtree: NodeKey,
}

// ============================================================================
// ContainerTree Implementation
// ============================================================================

impl<W: LayoutElement> ContainerArena<W> {
    fn tiled_window_key(&self, workspace: NodeKey, id: &W::Id) -> Option<NodeKey> {
        let key = self.window_key(id)?;
        (self.branch_root(key) == workspace).then_some(key)
    }

    /// Match sway's `view_map` arrange boundary after inserting a new tiled window.
    ///
    /// A view inserted below a container arranges that container against its current pending
    /// box. A view inserted directly below the workspace node arranges the
    /// whole workspace instead. Keeping this decision here also avoids reconstructing a path:
    /// the inserted leaf and its parent are already stable `NodeKey`s.
    fn layout_after_tiled_insert(&mut self, workspace: NodeKey, id: &W::Id) {
        let root = workspace;
        let parent = self
            .tiled_window_key(workspace, id)
            .and_then(|key| self.parent_of(key));

        if let Some(parent) = parent.filter(|parent| *parent != root) {
            self.layout_container_subtree(parent);
        } else {
            self.layout(workspace);
        }
    }

    fn render_fullscreen_window(&self, workspace: NodeKey) -> Option<W::Id> {
        let id = self.pending_fullscreen_window(workspace)?;
        let key = self.tiled_window_key(workspace, id)?;
        let tile = self.get_tile(key)?;
        tile.window()
            .sizing_mode()
            .is_fullscreen()
            .then(|| id.clone())
    }

    fn tiled_fullscreen_key(&self, workspace: NodeKey) -> Option<NodeKey> {
        let key = self.fullscreen_key(workspace)?;
        (self.holds_node(key) && self.branch_root(key) == workspace).then_some(key)
    }

    /// A tiled container that is currently the workspace fullscreen authority.
    ///
    /// Unlike a fullscreen leaf, this has no client commit to wait for: its descendants stay
    /// ordinary tiled clients and the compositor makes their branch exclusive itself.
    fn render_fullscreen_container(&self, workspace: NodeKey) -> Option<NodeKey> {
        let key = self.tiled_fullscreen_key(workspace)?;
        self.container_info(key)?;
        Some(key)
    }

    fn fullscreen_render_state(&self, workspace: NodeKey) -> FullscreenRenderState<W::Id> {
        let container = self.render_fullscreen_container(workspace);
        let fullscreen_id = self.render_fullscreen_window(workspace);
        let windowed_fullscreen_id = if fullscreen_id.is_none() && container.is_none() {
            self.focused_tile(workspace).and_then(|tile| {
                tile.window()
                    .is_pending_windowed_fullscreen()
                    .then(|| tile.window().id().clone())
            })
        } else {
            None
        };
        FullscreenRenderState {
            container,
            fullscreen_id,
            windowed_fullscreen_id,
        }
    }

    fn pending_fullscreen_window(&self, workspace: NodeKey) -> Option<&W::Id> {
        self.tiled_fullscreen_key(workspace)?;
        self.fullscreen_leaf_window_id(workspace)
    }

    pub(super) fn incoming_tile<'a>(
        &'a self,
        incoming: &'a IncomingView<W>,
    ) -> Option<&'a Tile<W>> {
        match incoming {
            IncomingView::Detached(view) => Some(view.tile()),
            IncomingView::Parked(key) => self.parked_tile(*key),
        }
    }

    pub(super) fn incoming_tile_mut<'a>(
        &'a mut self,
        incoming: &'a mut IncomingView<W>,
    ) -> Option<&'a mut Tile<W>> {
        match incoming {
            IncomingView::Detached(view) => Some(view.tile_mut()),
            IncomingView::Parked(key) => self.parked_tile_mut(*key),
        }
    }

    pub(super) fn window_id_with_mark(&self, mark: &str) -> Option<W::Id> {
        let key = self.node_with_mark(mark)?;
        self.representative_window_for_node(key)
            .map(|window| window.id().clone())
    }

    pub(super) fn swap_selected_with_mark(&mut self, mark: &str) -> bool {
        let Some(target) = self.node_with_mark(mark) else {
            return false;
        };
        self.swap_selected_nodes(target)
    }

    /// Arrange the two parents that `container_swap` leaves behind.
    ///
    /// sway's command calls `arrange_node` on each new parent, not
    /// `arrange_workspace`. That distinction is observable when the exchanged nodes live
    /// below a fullscreen ancestor: both parent containers retain their zero pending boxes.
    /// A workspace parent still expands to the regular whole-workspace arrange.
    fn arrange_swapped_parents(&mut self, selected: NodeKey, target: NodeKey) {
        let Some(workspace) = self.workspace_of(selected) else {
            return;
        };
        let current_parent = self.parent_of(selected);
        let other_parent = self.parent_of(target);

        let mut arrange = |parent| {
            if parent == workspace {
                self.layout(workspace);
            } else {
                self.layout_container_subtree(parent);
            }
        };
        if let Some(parent) = current_parent {
            arrange(parent);
        }
        if let Some(parent) = other_parent.filter(|parent| Some(*parent) != current_parent) {
            arrange(parent);
        }
        // `arrange_container` computes visibility inside that subtree. IPC visibility still
        // includes tabbed/stacked ancestors above it, which may not have been part of either
        // arrange (notably a fullscreen wrapper swapped inside a zero-sized stack).
        self.refresh_focus_visibility();
    }

    fn swap_selected_nodes(&mut self, target: NodeKey) -> bool {
        let Some(workspace) = self.workspace_of(target) else {
            return false;
        };
        let Some(selected) = self.selected_node_key(workspace) else {
            return false;
        };
        if !self.swap_nodes(selected, target) {
            return false;
        }
        self.arrange_swapped_parents(selected, target);
        true
    }

    fn swap_selected_at_floating_boundary(&mut self, target: NodeKey) -> bool {
        let Some(workspace) = self.workspace_of(target) else {
            return false;
        };
        let Some(selected) = self.selected_node_key(workspace) else {
            return false;
        };
        if !self.swap_nodes_at_floating_boundary(selected, target) {
            return false;
        }
        self.arrange_swapped_parents(selected, target);
        true
    }

    pub(super) fn swap_selected_with_window_at_floating_boundary(
        &mut self,
        target: &W::Id,
    ) -> bool {
        let Some(target) = self.window_key(target) else {
            return false;
        };
        self.swap_selected_at_floating_boundary(target)
    }

    pub(super) fn swap_selected_with_mark_at_floating_boundary(&mut self, mark: &str) -> bool {
        let Some(target) = self.node_with_mark(mark) else {
            return false;
        };
        self.swap_selected_at_floating_boundary(target)
    }

    /// The tiled side's cached leaf layouts. The floating groups are branches of the same
    /// tree and are laid out by the same pass, so "the layouts" has to say which branch.
    fn display_layouts(
        &self,
        workspace: NodeKey,
    ) -> impl DoubleEndedIterator<Item = &LeafLayoutInfo> + '_ {
        branch_display_layouts(self, workspace)
    }

    fn focused_key(&self, workspace: NodeKey) -> Option<NodeKey> {
        self.focus_inactive_view_in_branch(workspace)
    }

    pub(in crate::layout) fn focused_tile(&self, workspace: NodeKey) -> Option<&Tile<W>> {
        self.focused_key(workspace)
            .and_then(|key| self.get_tile(key))
    }

    fn effective_tab_bar_config(&self, workspace: NodeKey) -> TabBar {
        self.options(workspace).layout.tab_bar.clone()
    }

    /// Whether the workspace itself is what the tree currently has selected.
    ///
    /// An empty workspace has nothing else a command could be aimed at; otherwise this is
    /// what `focus parent` on a top-level node leaves behind. A focused *leaf* never selects
    /// the workspace, even when its parent is the workspace — measured against sway 1.11,
    fn available_span(&self, workspace: NodeKey, total: f64, child_count: usize) -> f64 {
        available_span(self.options(workspace).layout.gaps, total, child_count)
    }

    /// The gap between a branch's children.
    ///
    /// `gaps` is a workspace setting, and a floating group is not laid out in the workspace's
    /// box: it is laid out in its own, which is exactly the size the user dragged it to. A gap
    /// inside that box would come out of the window rather than out of the workspace.
    pub(super) fn branch_gap(&self, branch: NodeKey) -> f64 {
        let Some(workspace) = self.workspace_of(branch) else {
            return 0.;
        };
        if branch == workspace {
            self.options(workspace).layout.gaps
        } else {
            0.0
        }
    }

    fn available_span_in(&self, branch: NodeKey, total: f64, child_count: usize) -> f64 {
        available_span(self.branch_gap(branch), total, child_count)
    }

    /// What a `resize` in hundredths is measured against.
    ///
    /// sway climbs from the node's parent to the nearest ancestor whose layout is parallel to
    /// the axis and takes *its* width, falling back to the workspace's when there is none
    /// (`resize_set_tiled`, sway/commands/resize.c:290-300 and :315-325). That ancestor is not
    /// necessarily the container that will pay for the resize, so this is a separate climb
    /// from `find_resize_parent`.
    pub(super) fn ppt_reference(&self, key: NodeKey, layout: Layout) -> f64 {
        let Some(workspace) = self.workspace_of(key) else {
            return 0.;
        };
        self.find_parent_with_layout(key, layout)
            .and_then(|(parent, _)| self.node_span(parent, layout))
            .unwrap_or_else(|| match layout {
                Layout::SplitH => self.working_area(workspace).size.w,
                Layout::SplitV => self.working_area(workspace).size.h,
                Layout::Tabbed | Layout::Stacked => 0.0,
            })
    }

    fn resolve_preset_dimension(available: f64, preset: PresetSize) -> f64 {
        match preset {
            PresetSize::Proportion(prop) => {
                let proportion = if prop.abs() > 1.0 {
                    (prop / 100.0).clamp(0.0, 1.0)
                } else {
                    prop.clamp(0.0, 1.0)
                };
                available * proportion
            }
            PresetSize::Fixed(px) => px as f64,
        }
    }

    fn cycle_presets(
        &self,
        available: f64,
        current_percent: f64,
        presets: &[PresetSize],
        forwards: bool,
    ) -> Option<f64> {
        if presets.is_empty() || available <= 0.0 {
            return None;
        }

        let resolved: Vec<f64> = presets
            .iter()
            .map(|preset| Self::resolve_preset_dimension(available, *preset))
            .collect();

        if resolved.is_empty() {
            return None;
        }

        let epsilon = 0.5;
        let current_width = current_percent * available;

        let target_width = if forwards {
            resolved
                .iter()
                .copied()
                .find(|width| *width > current_width + epsilon)
                .unwrap_or_else(|| resolved[0])
        } else {
            resolved
                .iter()
                .copied()
                .rev()
                .find(|width| *width + epsilon < current_width)
                .unwrap_or_else(|| *resolved.last().unwrap())
        };

        Some((target_width / available).clamp(0.0, 1.0))
    }

    /// The node a window-addressed command targets: the named window, or the current
    /// selection when none is given.
    fn window_target(&self, workspace: NodeKey, window: Option<&W::Id>) -> Option<NodeKey> {
        match window {
            Some(id) => self.tiled_window_key(workspace, id),
            None => self.branch_position(workspace),
        }
    }

    fn window_container_metrics(&self, key: NodeKey, layout: Layout) -> Option<ContainerMetrics> {
        let (parent_key, child_idx) = self.find_parent_with_layout(key, layout)?;
        self.container_metrics(parent_key, child_idx, layout)
    }

    fn resize_container_metrics(
        &self,
        key: NodeKey,
        layout: Layout,
        reach: ResizeReach,
    ) -> Option<ContainerMetrics> {
        let (parent_key, child_idx) = self.find_resize_parent(key, layout, reach)?;
        let (container_layout, rect, child_count) = self.container_info(parent_key)?;
        if container_layout != layout || child_count == 0 {
            return None;
        }
        let available = self.child_resize_total(parent_key, child_idx, layout)?;
        (available > 0.0).then_some((parent_key, child_idx, available, child_count, rect))
    }

    /// The container a resize would act on, and the span its children share.
    ///
    /// Answered for whichever branch `parent_key` is in, because the two differ only in the
    /// gap between children.
    fn container_metrics(
        &self,
        parent_key: NodeKey,
        child_idx: usize,
        layout: Layout,
    ) -> Option<ContainerMetrics> {
        let (container_layout, rect, child_count) = self.container_info(parent_key)?;
        if container_layout != layout || child_count == 0 {
            return None;
        }

        let branch = self.branch_root(parent_key);
        let available = match layout {
            Layout::SplitH => self.available_span_in(branch, rect.size.w, child_count),
            Layout::SplitV => self.available_span_in(branch, rect.size.h, child_count),
            Layout::Tabbed | Layout::Stacked => return None,
        };

        if available <= 0.0 {
            return None;
        }

        Some((parent_key, child_idx, available, child_count, rect))
    }

    /// The same, reached from a node rather than from its container.
    pub(super) fn container_metrics_for(
        &self,
        key: NodeKey,
        layout: Layout,
    ) -> Option<ContainerMetrics> {
        let (parent_key, child_idx) = self.find_parent_with_layout(key, layout)?;
        self.container_metrics(parent_key, child_idx, layout)
    }

    fn selected_geometry(&self, workspace: NodeKey) -> Option<Rectangle<f64, Logical>> {
        let key = self.selected_layout_parent_key()?;

        // For container selection visuals, prefer the on-screen leaf geometry under this
        // container. This stays in sync with what is currently rendered even when the
        // container's cached geometry is in transition.
        let mut bounds: Option<Rectangle<f64, Logical>> = None;
        for info in self
            .display_layouts(workspace)
            .filter(|info| self.is_descendant(info.key, key))
        {
            bounds = Some(match bounds {
                Some(acc) => {
                    let left = acc.loc.x.min(info.rect.loc.x);
                    let top = acc.loc.y.min(info.rect.loc.y);
                    let right = (acc.loc.x + acc.size.w).max(info.rect.loc.x + info.rect.size.w);
                    let bottom = (acc.loc.y + acc.size.h).max(info.rect.loc.y + info.rect.size.h);
                    Rectangle::new(
                        Point::from((left, top)),
                        Size::from((right - left, bottom - top)),
                    )
                }
                None => info.rect,
            });
        }

        // A tabbed or stacked container shows one child at a time, so the leaves under it
        // add up to that child's box and nothing else — the same box a plain focused window
        // would have. Selecting the container then looked exactly like selecting the window
        // inside it. Its tab bar is the part of it that is always on screen and always its
        // own, so the selection reaches up over it: what is highlighted is the whole
        // switcher, which is what `focus parent` selected.
        if let Some((layout, geometry, _)) = self.container_info(key) {
            if matches!(layout, Layout::Tabbed | Layout::Stacked) {
                bounds = Some(match bounds {
                    Some(acc) => {
                        let top = acc.loc.y.min(geometry.loc.y);
                        Rectangle::new(
                            Point::from((acc.loc.x, top)),
                            Size::from((acc.size.w, acc.loc.y + acc.size.h - top)),
                        )
                    }
                    None => geometry,
                });
            }
        }

        bounds.or_else(|| self.container_info(key).map(|(_, rect, _)| rect))
    }

    pub fn selected_is_container(&self, workspace: NodeKey) -> bool {
        self.selected_container_in(workspace)
    }

    /// Whether a child-laying node is selected in the tiled branch. Unlike the command-facing
    /// container query, this includes the workspace node so `focus parent` always has a visual
    /// result.
    fn selected_layout_parent_in_tiling(&self, workspace: NodeKey) -> bool {
        self.selected_layout_parent_key()
            .is_some_and(|key| self.branch_root(key) == workspace)
    }

    // ── Commands, asked of a branch ──────────────────────────────────────────────────
    //
    // sway's commands take a container and walk from it, so there is exactly one of each.
    // Tiri's took the tree and started at its root, and once the floating groups became
    // branches of the same tree that meant a second copy of every one of them, differing in
    // which root it started from and in nothing else. These take the root.
    //
    // Each arranges when it changed something. One arrange pass covers both sides of the
    // workspace, so which branch was touched does not change what has to be recomputed —
    // only whether anything does.

    /// Whether the selection is a container in this branch.
    ///
    /// A floating root is parented to the workspace so that `focus parent` can climb out of
    /// it, which makes every floating node a descendant of the workspace root. Branch
    /// membership is therefore the question to ask, not ancestry: otherwise a selected
    /// floating container answers for the tiled side as well as its own.
    pub(super) fn selected_container_in(&self, branch: NodeKey) -> bool {
        self.selected_container_key()
            .is_some_and(|key| self.branch_root(key) == branch)
    }

    /// The layout a command in this branch would be read against: the selected container's
    /// when there is one, otherwise the layout of whatever holds the branch's focus.
    pub(super) fn selection_layout_in(&self, branch: NodeKey) -> Option<Layout> {
        let key = self.branch_position(branch)?;
        if self.selected_container_in(branch) {
            if let Some(info) = self.container_info(key) {
                return Some(info.0);
            }
        }
        self.layout_owning(key)
    }

    /// Whether this branch is nothing but the window in it.
    ///
    /// `ws->floating` holds whatever was floated, a view included, so a floating window has no
    /// container around it until a command builds one. A `layout` aimed here therefore has
    /// nothing to act on, and cycling between groups is cycling between windows. The workspace
    /// is never this: it is a container sway really has, however few children it holds.
    ///
    /// This used to ask whether the root was a wrapper tiri had added, which is the same
    /// question only for as long as there is a wrapper to recognise.
    pub(super) fn branch_root_is_lone_window(&self, branch: NodeKey) -> bool {
        let Some(workspace) = self.workspace_of(branch) else {
            return false;
        };
        branch != workspace && self.is_leaf(branch)
    }

    /// A layout command needs a container to act on. Vacuous on the tiled side, where the
    /// branch root is the workspace.
    fn branch_has_layout_target(&self, branch: NodeKey) -> bool {
        self.selection_layout_in(branch).is_some() && !self.branch_root_is_lone_window(branch)
    }

    pub(super) fn split_in_branch(&mut self, branch: NodeKey, layout: Layout) -> bool {
        let Some(workspace) = self.workspace_of(branch) else {
            return false;
        };
        let target = self.command_target_in(branch);
        let changed = self.split_target(layout, target);
        if changed {
            self.layout(workspace);
        }
        changed
    }

    pub(super) fn unsplit_in_branch(&mut self, branch: NodeKey) -> Option<NodeKey> {
        let workspace = self.workspace_of(branch)?;
        let target = self.command_target_in(branch);
        let root = self.unsplit_target(target)?;
        self.layout(workspace);
        Some(root)
    }

    pub(super) fn set_layout_in_branch(&mut self, branch: NodeKey, layout: Layout) -> bool {
        let Some(workspace) = self.workspace_of(branch) else {
            return false;
        };
        if !self.branch_has_layout_target(branch) {
            return false;
        }
        let target = self.command_target_in(branch);
        let changed = self.set_layout_for_target(layout, target);
        if changed {
            self.layout(workspace);
        }
        changed
    }

    pub(super) fn toggle_split_in_branch(&mut self, branch: NodeKey) -> bool {
        let Some(workspace) = self.workspace_of(branch) else {
            return false;
        };
        if !self.branch_has_layout_target(branch) {
            return false;
        }
        let target = self.command_target_in(branch);
        let changed = self.toggle_split_for_target(target);
        if changed {
            self.layout(workspace);
        }
        changed
    }

    pub(super) fn toggle_layout_all_in_branch(&mut self, branch: NodeKey) -> bool {
        let Some(workspace) = self.workspace_of(branch) else {
            return false;
        };
        if !self.branch_has_layout_target(branch) {
            return false;
        }
        let target = self.command_target_in(branch);
        let changed = self.toggle_layout_all_for_target(target);
        if changed {
            self.layout(workspace);
        }
        changed
    }

    pub(super) fn set_default_layout_in_branch(&mut self, branch: NodeKey) -> bool {
        let Some(workspace) = self.workspace_of(branch) else {
            return false;
        };
        if !self.branch_has_layout_target(branch) {
            return false;
        }
        let target = self.command_target_in(branch);
        let changed = self.set_default_layout_for_target(target);
        if changed {
            self.layout(workspace);
        }
        changed
    }

    pub(super) fn toggle_layout_cycle_in_branch(
        &mut self,
        branch: NodeKey,
        cycle: &[LayoutCycleEntry],
    ) -> bool {
        let Some(workspace) = self.workspace_of(branch) else {
            return false;
        };
        if !self.branch_has_layout_target(branch) {
            return false;
        }
        let target = self.command_target_in(branch);
        let changed = self.toggle_layout_cycle_for_target(target, cycle);
        if changed {
            self.layout(workspace);
        }
        changed
    }

    /// What `close` would close, aimed at this branch.
    pub(super) fn close_window_ids_in_branch(&self, branch: NodeKey) -> Vec<W::Id> {
        if self.selected_container_in(branch) {
            if let Some(key) = self.selected_container_key() {
                return self.window_ids_under(key);
            }
        }
        self.focused_window_in_branch(branch)
            .map(|window| vec![window.id().clone()])
            .unwrap_or_default()
    }

    pub(super) fn close_window_ids_for_active_selection(&self, workspace: NodeKey) -> Vec<W::Id> {
        if self.selected_is_container(workspace) {
            if let Some(key) = self.selected_container_key() {
                return self.window_ids_under(key);
            }
        }

        self.active_window(workspace)
            .map(|window| vec![window.id().clone()])
            .unwrap_or_default()
    }

    pub(super) fn take_selected_subtree(
        &self,
        workspace: NodeKey,
    ) -> Option<(NodeKey, Rectangle<f64, Logical>)> {
        let key = self.selected_node_key(workspace)?;
        self.container_info(key)?;
        let rect = self.default_floating_container_rect(workspace);
        Some((key, rect))
    }

    /// sway's initial geometry for a container without a view when it starts floating.
    fn default_floating_container_rect(&self, workspace: NodeKey) -> Rectangle<f64, Logical> {
        let area = self.working_area(workspace);
        let size = Size::from((area.size.w * 0.5, area.size.h * 0.75));
        Rectangle::new(
            Point::from((
                area.loc.x + (area.size.w - size.w) / 2.0,
                area.loc.y + (area.size.h - size.h) / 2.0,
            )),
            size,
        )
    }

    /// The whole tiling workspace as one subtree, for floating it in one piece.
    pub(super) fn take_workspace_subtree_for_floating(
        &self,
        workspace: NodeKey,
    ) -> Option<(NodeKey, Rectangle<f64, Logical>)> {
        // What sway floats here is a container, and a container's default floating size is
        // `container_floating_set_default_size`: half the workspace by three quarters of it,
        // centred. `floating_natural_resize` overwrites that with the client's own size only
        // when there is a view to ask, and a wrapper around the workspace's children is not
        // one — which is why the number a floating *window* no longer gets is still this
        // one's.
        let rect = self.default_floating_container_rect(workspace);
        (!self.workspace_is_empty(workspace)).then_some((workspace, rect))
    }

    pub(super) fn subtree_for_window_floating(
        &self,
        workspace: NodeKey,
        id: &W::Id,
    ) -> Option<(NodeKey, Rectangle<f64, Logical>)> {
        let key = self.tiled_window_key(workspace, id)?;
        let rect = self
            .fullscreen_restore_geometry(key)
            .filter(|rect| rect.size.w > 0.0 && rect.size.h > 0.0)
            .or_else(|| {
                self.display_layouts(workspace)
                    .find(|info| info.key == key)
                    .map(|info| info.rect)
            })?;
        Some((key, rect))
    }

    /// Run a mutation on the tree and relayout when it reports a change.
    ///
    /// Every structural mutation must be followed by a relayout; routing them through this
    /// combinator makes that impossible to forget.
    fn mutate_tree<R: TreeMutation>(
        &mut self,
        workspace: NodeKey,
        f: impl FnOnce(&mut ContainerArena<W>) -> R,
    ) -> R {
        let result = f(self);
        if result.changed() {
            self.layout(workspace);
        }
        result
    }

    /// Per-leaf resize edges for an in-flight interactive resize, keyed by node.
    ///
    /// Computed up front so the caller can hold mutable tile borrows while iterating.
    fn interactive_resize_data_by_leaf(
        &self,
        resize: &Option<InteractiveResizeState<W>>,
        layouts: &[LeafFrameInfo],
    ) -> HashMap<NodeKey, InteractiveResizeData> {
        let Some(resize) = resize.as_ref() else {
            return HashMap::new();
        };

        layouts
            .iter()
            .filter_map(|info| {
                let edges = self.resize_edges_for_leaf(
                    info.key,
                    resize.horizontal.as_ref(),
                    resize.vertical.as_ref(),
                );
                (!edges.is_empty()).then_some((info.key, InteractiveResizeData { edges }))
            })
            .collect()
    }

    /// Apply one axis of an in-flight interactive resize. `delta` is the pointer movement
    /// along that axis; `inverted` flips it for drags on the leading edge, where moving the
    /// pointer left/up grows the window.
    fn apply_interactive_resize(
        &mut self,
        target: Option<&ResizeTarget>,
        layout: Layout,
        delta: f64,
        inverted: bool,
    ) -> bool {
        let Some(target) = target else {
            return false;
        };
        let Some(available) = self.resize_available_span(target, layout) else {
            return false;
        };

        let delta = if inverted { -delta } else { delta };
        let new_span = (target.original_span.max(1.0) + delta).round() as i32;
        let current_percent = self.resize_current_percent(target);
        let percent = percent_from_size_change(
            current_percent,
            available,
            || self.ppt_reference(target.child, layout),
            SizeChange::SetFixed(new_span),
        );

        self.apply_resize(target, layout, percent)
    }

    /// Add a workspace root to this arena and return it.
    ///
    /// `Workspace` no longer builds a tree of its own: it is handed the one tree and takes a
    /// root in it. That is what makes a workspace moving between outputs a reparent rather
    /// than a subtree carried between two arenas.
    pub fn add_workspace(
        &mut self,
        view_size: Size<f64, Logical>,
        working_area: Rectangle<f64, Logical>,
        scale: f64,
        options: Rc<Options>,
    ) -> NodeKey {
        let background_color = options.layout.background_color;
        let workspace =
            self.add_workspace_node(Layout::SplitH, view_size, working_area, scale, options);
        self.render.overview.insert(
            workspace,
            WorkspaceOverview::new(view_size, background_color),
        );
        workspace
    }

    /// Every window under the given workspace roots, in their order.
    ///
    /// One pass over the arena rather than one borrow per workspace: the workspaces share a
    /// tree now, so they cannot each be handed a mutable borrow of it in turn.
    pub fn windows_mut_in_workspaces(&mut self, workspaces: &[NodeKey]) -> Vec<&mut W> {
        self.tiles_mut_in_workspaces(workspaces)
            .into_iter()
            .map(Tile::window_mut)
            .collect()
    }

    /// Every tile under the given workspace roots, both sides, in their order.
    pub fn tiles_mut_in_workspaces(&mut self, workspaces: &[NodeKey]) -> Vec<&mut Tile<W>> {
        let keys: Vec<NodeKey> = workspaces
            .iter()
            .flat_map(|workspace| self.leaf_keys_in_workspace(*workspace))
            .collect();
        self.tiles_mut_for_keys(&keys)
            .into_iter()
            .map(|(_, tile)| tile)
            .collect()
    }

    // Basic getters using ContainerTree
    /// The windows on one workspace's tiled side, in visual order.
    ///
    /// The tiled side only, like [`Self::tiles_mut`]. A workspace's two sides together are
    /// [`super::workspace::Workspace::windows`], which walks its floating groups as well.
    pub fn windows(&self, workspace: NodeKey) -> impl Iterator<Item = &W> + '_ {
        self.windows_in_branch(workspace).into_iter()
    }

    /// The tiles on one workspace's tiled side, in visual order.
    pub fn tiles(&self, workspace: NodeKey) -> impl Iterator<Item = &Tile<W>> + '_ {
        self.tiles_in_branch(workspace).into_iter()
    }

    pub fn active_window_mut(&mut self, workspace: NodeKey) -> Option<&mut W> {
        let key = self.focus_inactive_view_in_branch(workspace)?;
        Some(self.get_tile_mut(key)?.window_mut())
    }

    pub fn is_active_pending_fullscreen(&self, workspace: NodeKey) -> bool {
        self.fullscreen_key(workspace).is_some()
            || self
                .focused_tile(workspace)
                .is_some_and(|tile| tile.window().is_pending_windowed_fullscreen())
    }

    pub fn parent_area(&self, workspace: NodeKey) -> Rectangle<f64, Logical> {
        self.working_area(workspace)
    }

    /// Whether one of a workspace's two sides is the focused one.
    pub(in crate::layout) fn side_is_active(&self, workspace: NodeKey, floating: bool) -> bool {
        self.active_side.get(&workspace) == Some(&floating)
    }

    /// Whose the focus is: the workspace's, and then which of its two sides has it.
    ///
    /// The owner says, because only the owner knows — a side asked about itself would only
    /// ever answer yes.
    pub(super) fn set_active(
        &mut self,
        workspace: NodeKey,
        is_active: bool,
        floating_has_focus: bool,
    ) {
        if is_active {
            self.active_side.insert(workspace, floating_has_focus);
        } else {
            self.active_side.remove(&workspace);
        }
    }

    pub fn clock(&self) -> &Clock {
        &self.clock
    }

    pub fn verify_invariants(&self, workspace: NodeKey) {
        // Every workspace, and then the arena: the checks that are about the arena rather than
        // about one workspace — reachability, the seat's focused leaf, the layout caches — can
        // only be made where every root has been walked.
        self.verify_all_invariants();

        // The same check the floating side makes on its own tiles. A tiled tile on a foreign
        // options handle is invisible until something floats it, so the two sides ask it.
        for tile in self.tiles(workspace) {
            assert!(
                Rc::ptr_eq(self.options(workspace), &tile.options),
                "a tiled tile must be on its workspace's options: ws={:?} tile={:?} ws_opts={:p} tile_opts={:p}",
                workspace,
                tile.window().id(),
                Rc::as_ptr(self.options(workspace)),
                Rc::as_ptr(&tile.options),
            );
            assert_eq!(self.view_size(workspace), tile.view_size());
            assert_eq!(self.scale(workspace), tile.scale());
        }
    }

    /// Whether any container in this space uses `layout`.
    ///
    /// Shape assertions use this instead of searching the debug dump for a layout name,
    /// which also matches window titles and changes meaning with the dump's format.
    /// Window ids in visual (depth-first) order.
    #[cfg(test)]
    #[cfg(test)]
    pub fn all_window_ids(&self, workspace: NodeKey) -> Vec<W::Id> {
        self.window_ids_in_workspace(workspace)
    }

    /// Number of children directly under the tree root.
    /// The focused window's id, for shape assertions that used to look for a `*` marker in
    /// the debug dump.
    #[cfg(test)]
    pub fn focused_window_id(&self, workspace: NodeKey) -> Option<W::Id> {
        let key = self.focused_node_key()?;
        (self.workspace_of(key) == Some(workspace))
            .then(|| self.get_tile(key))
            .flatten()
            .map(|tile| tile.window().id().clone())
    }

    /// Whether the tree holds any container at all, as opposed to a lone window.
    #[cfg(test)]
    pub fn has_containers(&self, workspace: NodeKey) -> bool {
        [
            Layout::SplitH,
            Layout::SplitV,
            Layout::Tabbed,
            Layout::Stacked,
        ]
        .into_iter()
        .any(|layout| self.contains_layout(workspace, layout))
    }

    #[cfg(test)]
    pub fn debug_workspace_layout(&self, workspace: NodeKey) -> Layout {
        self.workspace_layout(workspace)
    }

    #[cfg(test)]
    pub fn debug_root_is_workspace_node(&self, workspace: NodeKey) -> bool {
        self.is_workspace(workspace)
    }

    #[cfg(test)]
    pub fn debug_selection_visual_geometry(
        &self,
        workspace: NodeKey,
    ) -> Option<Rectangle<f64, Logical>> {
        self.selected_layout_parent_in_tiling(workspace)
            .then(|| self.selected_geometry(workspace))
            .flatten()
    }

    pub fn update_window(
        &mut self,
        workspace: NodeKey,
        window: &W::Id,
        serial: Option<smithay::utils::Serial>,
    ) {
        let Some(key) = self.tiled_window_key(workspace, window) else {
            return;
        };
        let Some(tile) = self.get_tile_mut(key) else {
            return;
        };

        // Do this before calling update_window() so it can get up-to-date info.
        if let Some(serial) = serial {
            tile.window_mut().on_commit(serial);
        }

        tile.update_window();
    }

    pub fn render_elements<R: NiriRenderer>(
        &self,
        workspace: NodeKey,
        closing: &[ClosingWindow],
        mut ctx: RenderCtx<R>,
        xray_pos: XrayPos,
        tiling_focus_ring: bool,
        layer: RenderLayer,
    ) -> Vec<ContainerTreeRenderElement<R>> {
        // Pre-allocate: ~4 elements per tile + closing windows + tab bars
        let tile_count = self.window_count();
        let estimated_capacity = tile_count * 4 + closing.len() + tile_count / 2;
        let mut elements = Vec::with_capacity(estimated_capacity);
        let mut active_elements = Vec::with_capacity(8);
        let scale = Scale::from(self.scale(workspace));
        let focused_key = self.focused_key(workspace);
        let selection_is_layout_parent = self.selected_layout_parent_in_tiling(workspace);
        let selection_is_active =
            self.side_is_active(workspace, false) || self.workspace_is_selected(workspace);
        let fullscreen = self.fullscreen_render_state(workspace);
        let view_rect = Rectangle::from_size(self.view_size(workspace));

        for closing in closing.iter().rev().filter(|_| layer.is_normal()) {
            let elem = closing.render(ctx.as_gles(), view_rect, scale);
            elements.push(ContainerTreeRenderElement::ClosingWindow(elem));
        }

        // Render container selection before regular tiling elements so it ends up
        // visually on top after the global reverse-order composition pass.
        if layer.is_normal()
            && selection_is_layout_parent
            && (tiling_focus_ring || selection_is_active)
        {
            if let Some(rect) = self.selected_geometry(workspace) {
                let mut selection_border = self.options(workspace).layout.border;
                if let Some(focus_info) = self
                    .display_layouts(workspace)
                    .find(|info| Some(info.key) == focused_key)
                {
                    if let Some(tile) = self.get_tile(focus_info.key) {
                        if let Some(width) = tile.effective_border_width() {
                            selection_border.width = width;
                        }
                    }
                }
                render_container_selection(
                    ctx.renderer,
                    rect,
                    view_rect,
                    self.scale(workspace),
                    selection_is_active,
                    self.options(workspace).layout.focus_ring,
                    selection_border,
                    &mut |elem| elements.push(ContainerTreeRenderElement::ContainerSelection(elem)),
                );
            }
        }

        let render_layouts: Vec<&LeafLayoutInfo> = self.display_layouts(workspace).collect();
        for info in render_layouts.iter().rev() {
            // Use O(1) key lookup instead of O(depth) path lookup.
            if let Some(tile) = self.get_tile(info.key) {
                // Skip tiles belonging to a different render layer.
                if layer.is_normal() == tile.is_moving_between_workspaces() {
                    continue;
                }
                let in_fullscreen_container = fullscreen
                    .container
                    .is_some_and(|scope| self.is_descendant(info.key, scope));
                let state = fullscreen.tile_state(
                    tile.window().id(),
                    info.visible,
                    in_fullscreen_container,
                );
                if !state.visible {
                    continue;
                }

                let mut pos = info.rect.loc + tile.render_offset();
                pos = pos.to_physical_precise_round(scale).to_logical(scale);
                if state.client_fullscreen {
                    pos = Point::from((0.0, 0.0));
                }

                let is_focused = self.side_is_active(workspace, false)
                    && Some(info.key) == focused_key
                    && !selection_is_layout_parent;
                let draw_focus = tiling_focus_ring && is_focused;
                let target_elements = if Some(info.key) == focused_key {
                    &mut active_elements
                } else {
                    &mut elements
                };
                let tile_xray_pos = xray_pos.offset(pos);
                tile.render(ctx.r(), pos, tile_xray_pos, draw_focus, &mut |elem| {
                    target_elements.push(ContainerTreeRenderElement::from(elem));
                });
            }
        }

        elements.extend(active_elements);

        if !fullscreen.has_client_fullscreen() && !self.options(workspace).layout.tab_bar.off {
            let tab_bar_infos = self.tab_bar_layouts(workspace);
            let mut cache = self.render.tab_bar_cache.borrow_mut();
            let gles = ctx.renderer.as_gles_renderer();
            let tab_bar_config = self.effective_tab_bar_config(workspace);
            let is_active_workspace = self.side_is_active(workspace, false);
            let target = ctx.target;
            for info in tab_bar_infos {
                if fullscreen
                    .container
                    .is_some_and(|scope| !self.is_descendant(info.key, scope))
                {
                    continue;
                }
                let state = tab_bar_state_from_info(
                    &info,
                    &tab_bar_config,
                    is_active_workspace,
                    self.scale(workspace),
                    target,
                );
                let (buffer, tab_widths_px) = match cache.get(&info.key) {
                    Some(entry) if entry.state == state => {
                        (entry.buffer.clone(), entry.tab_widths_px.clone())
                    }
                    _ => match render_tab_bar(
                        gles,
                        &tab_bar_config,
                        info.layout,
                        info.rect,
                        info.row_height,
                        &info.tabs,
                        is_active_workspace,
                        target,
                        self.scale(workspace),
                    ) {
                        Ok(TabBarRenderOutput {
                            buffer,
                            tab_widths_px,
                        }) => (buffer, tab_widths_px),
                        Err(err) => {
                            warn!("tab bar render failed: {err}");
                            continue;
                        }
                    },
                };

                let mut location = info.rect.loc;
                location = location.to_physical_precise_round(scale).to_logical(scale);
                let elem = TextureRenderElement::from_texture_buffer(
                    buffer.clone(),
                    location,
                    1.0,
                    None,
                    None,
                    Kind::Unspecified,
                );
                elements.push(ContainerTreeRenderElement::TabBar(
                    PrimaryGpuTextureRenderElement(elem),
                ));

                cache.insert(
                    info.key,
                    TabBarCacheEntry {
                        state,
                        buffer,
                        tab_widths_px,
                    },
                );
            }
        }

        elements
    }

    #[allow(clippy::too_many_arguments)]
    pub fn render<R: NiriRenderer>(
        &self,
        workspace: NodeKey,
        closing: &[ClosingWindow],
        ctx: RenderCtx<R>,
        xray_pos: XrayPos,
        tiling_focus_ring: bool,
        layer: RenderLayer,
        push: &mut dyn FnMut(ContainerTreeRenderElement<R>),
    ) {
        for elem in
            self.render_elements(workspace, closing, ctx, xray_pos, tiling_focus_ring, layer)
        {
            push(elem);
        }
    }

    pub fn render_as_offscreen(
        &self,
        workspace: NodeKey,
        closing: &[ClosingWindow],
        renderer: &mut GlesRenderer,
        target: RenderTarget,
        tiling_focus_ring: bool,
    ) -> Option<OffscreenRenderElement> {
        for tile in self.tiles(workspace) {
            tile.window().set_offscreen_data(None);
        }

        let ctx = RenderCtx {
            renderer,
            target,
            xray: None,
        };
        let mut elements = self.render_elements(
            workspace,
            closing,
            ctx,
            XrayPos::default(),
            tiling_focus_ring,
            RenderLayer::Normal,
        );
        if elements.is_empty() {
            return None;
        }

        let overview = self.render.overview.get(&workspace)?;
        let background = SolidColorRenderElement::from_buffer(
            &overview.background,
            Point::from((0., 0.)),
            1.,
            Kind::Unspecified,
        );
        elements.push(background.into());

        overview
            .offscreen
            .render(renderer, Scale::from(self.scale(workspace)), &elements)
            .map(|(elem, _sync, data)| {
                for tile in self.tiles(workspace) {
                    tile.window().set_offscreen_data(Some(data.clone()));
                }

                elem
            })
            .map_err(|err| warn!("error rendering tiling space to offscreen: {err:?}"))
            .ok()
    }

    // Layout operations using ContainerTree
    pub fn advance_animations(&mut self, workspace: NodeKey) {
        for tile in self.tiles_mut(workspace) {
            tile.advance_animations();
        }
    }

    pub fn are_animations_ongoing(&self, workspace: NodeKey) -> bool {
        self.tiles(workspace)
            .any(|tile| tile.are_animations_ongoing())
    }

    /// What a tile needs to know about the space it is in.
    ///
    /// Taken as a value so a caller can read it before borrowing the arena: a tile is reached
    /// through the tree, and asking the space about itself while holding one is the borrow
    /// checker's way of pointing out that these are two different questions.
    pub(super) fn tile_config(&self, workspace: NodeKey) -> TileConfig {
        TileConfig {
            view_size: self.view_size(workspace),
            scale: self.scale(workspace),
            options: self.options(workspace).clone(),
        }
    }

    pub(super) fn tab_bar_cache_mut(&self) -> RefMut<'_, HashMap<NodeKey, TabBarCacheEntry>> {
        self.render.tab_bar_cache.borrow_mut()
    }

    pub(super) fn update_render_elements(
        &mut self,
        workspace: NodeKey,
        resize: &Option<InteractiveResizeState<W>>,
    ) {
        let _span = tracy_client::span!("ContainerTree::update_render_elements");
        let is_active = self.side_is_active(workspace, false);
        // Once a frame, and for both sides: a container that has left the tree will not be
        // asked for again, and its texture is the largest thing an entry holds.
        self.render
            .tab_bar_cache
            .borrow_mut()
            .retain(|key, _| self.holds_node(*key));
        self.apply_pending_layouts_and_deferred_relayout(workspace);
        let has_pending = self.has_pending_layouts(workspace);
        let mut state_layouts = std::mem::take(&mut self.render.state_layout_scratch);
        state_layouts.clear();
        {
            let _span = tracy_client::span!("ContainerTree::project_state_layouts_for_render");
            if has_pending {
                state_layouts.extend(
                    self.pending_leaf_layouts()
                        .map(|(branch, info)| LeafFrameInfo::new(branch, info)),
                );
            } else {
                let arena = &*self;
                state_layouts.extend(
                    arena
                        .leaf_layouts_in(workspace)
                        .map(|info| LeafFrameInfo::new(arena.branch_root(info.key), info)),
                );
            }
        }
        #[cfg(feature = "profile-with-tracy")]
        {
            tracy_client::plot!("layout.state_leaf_projections", state_layouts.len() as f64);
        }
        let tiled_root = workspace;
        state_layouts.retain(|info| info.branch == tiled_root);
        let workspace_view = Rectangle::from_size(self.view_size(workspace));
        let focused_key = self.focused_key(workspace);
        let selection_is_layout_parent = self.selected_layout_parent_in_tiling(workspace);
        let scale = Scale::from(self.scale(workspace));
        let fullscreen = self.fullscreen_render_state(workspace);
        let fullscreen_container = fullscreen.container;
        let logical_fullscreen_id = self.pending_fullscreen_window(workspace).cloned();
        let layout_rect = self.layout_area(workspace);
        let is_single_window = self.window_count() <= 1;
        let mut render_layouts = std::mem::take(&mut self.render.render_layout_scratch);
        render_layouts.clear();
        {
            let _span = tracy_client::span!("ContainerTree::project_display_layouts_for_render");
            let branch = workspace;
            render_layouts.extend(
                self.display_layouts(workspace)
                    .map(|info| LeafFrameInfo::new(branch, info)),
            );
        }
        #[cfg(feature = "profile-with-tracy")]
        {
            tracy_client::plot!(
                "layout.render_leaf_projections",
                render_layouts.len() as f64
            );
        }
        let mut render_edges = std::mem::take(&mut self.render.render_edges_scratch);
        render_edges.clear();
        {
            let _span = tracy_client::span!("ContainerTree::collect_render_edges");
            render_edges.extend(render_layouts.iter().map(|info| {
                let mut edges = edge_visibility_for_tile(
                    self.options(workspace),
                    layout_rect,
                    info.rect,
                    self.scale(workspace),
                    is_single_window,
                );
                // The tab above this tile is its top decoration; the bar is grown to cover
                // the lane (`tab_bar_rect`), so drawing it too would stack two.
                if self.parent_is_switcher(info.key) {
                    edges.top = false;
                }
                let indicator_edge = split_indicator_edge_for_tile(self, info.key, edges);
                (edges, indicator_edge)
            }));
        }

        let options = self.options(workspace).clone();
        let ctx = WindowStateContext {
            focused_key,
            workspace_active: is_active,
            deactivate_unfocused: options.deactivate_unfocused_windows,
            request_size: !has_pending,
            working_area_size: self.working_area(workspace).size,
            options: &options,
            fullscreen_id: logical_fullscreen_id.as_ref(),
            windowed_fullscreen_id: fullscreen.windowed_fullscreen_id.as_ref(),
            has_fullscreen_container: fullscreen_container.is_some(),
            view_size: self.view_size(workspace),
        };
        // A stale snapshot describes a tree that no longer exists; driving window state
        // from it would flush configures carrying its obsolete bounds. The deferred
        // relayout will run this pass again once the transaction resolves.
        let skip_state_pass = self.pending_layout_is_stale(workspace);
        let resize_data = self.interactive_resize_data_by_leaf(resize, &state_layouts);
        for info in &state_layouts {
            if skip_state_pass {
                break;
            }
            let in_fullscreen_container =
                fullscreen_container.is_some_and(|scope| self.is_descendant(info.key, scope));
            // Use O(1) key lookup instead of O(depth) path lookup.
            if let Some(tile) = self.get_tile_mut(info.key) {
                let resize = resize_data.get(&info.key).copied();
                Self::update_window_state(tile, info, resize, in_fullscreen_container, &ctx);
            }
        }

        for (info, (edges, indicator_edge)) in render_layouts
            .iter()
            .copied()
            .zip(render_edges.iter().copied())
        {
            let is_in_fullscreen_container =
                fullscreen_container.is_some_and(|scope| self.is_descendant(info.key, scope));
            // Asked before the tile is borrowed mutably below.
            let is_focus_head = self.is_focus_head(info.key);
            // Use O(1) key lookup instead of O(depth) path lookup.
            if let Some(tile) = self.get_tile_mut(info.key) {
                let state = fullscreen.tile_state(
                    tile.window().id(),
                    info.visible,
                    is_in_fullscreen_container,
                );

                let mut pos = info.rect.loc + tile.render_offset();
                pos = pos.to_physical_precise_round(scale).to_logical(scale);
                if state.client_fullscreen {
                    pos = Point::from((0.0, 0.0));
                }

                let mut tile_view_rect = workspace_view;
                tile_view_rect.loc -= pos;

                if state.client_fullscreen {
                    tile_view_rect = workspace_view;
                }

                if state.visible {
                    let is_focused =
                        is_active && Some(info.key) == focused_key && !selection_is_layout_parent;
                    tile.update_render_elements(
                        is_active,
                        is_focused,
                        is_focus_head,
                        edges,
                        indicator_edge,
                        tile_view_rect,
                    );
                }
            }
        }

        self.render.state_layout_scratch = state_layouts;
        self.render.render_layout_scratch = render_layouts;
        self.render.render_edges_scratch = render_edges;
    }

    pub(super) fn interactive_resize_begin(
        &mut self,
        workspace: NodeKey,
        resize: &mut Option<InteractiveResizeState<W>>,
        window: W::Id,
        edges: ResizeEdge,
    ) -> bool {
        self.interactive_resize_begin_internal(workspace, resize, window, edges, None)
    }

    pub(super) fn interactive_resize_begin_at(
        &mut self,
        workspace: NodeKey,
        resize: &mut Option<InteractiveResizeState<W>>,
        window: W::Id,
        edges: ResizeEdge,
        pos: Point<f64, Logical>,
    ) -> bool {
        self.interactive_resize_begin_internal(workspace, resize, window, edges, Some(pos))
    }

    fn interactive_resize_begin_internal(
        &mut self,
        workspace: NodeKey,
        resize: &mut Option<InteractiveResizeState<W>>,
        window: W::Id,
        edges: ResizeEdge,
        pos: Option<Point<f64, Logical>>,
    ) -> bool {
        if resize.is_some() {
            return false;
        }

        let Some((edges, horizontal, vertical)) =
            self.resize_targets_for_window(&window, edges, pos)
        else {
            return false;
        };

        *resize = Some(InteractiveResizeState {
            window,
            data: InteractiveResizeData { edges },
            horizontal,
            vertical,
        });
        self.layout(workspace);
        true
    }

    pub(super) fn interactive_resize_update(
        &mut self,
        workspace: NodeKey,
        resize: &Option<InteractiveResizeState<W>>,
        window: &W::Id,
        delta: Point<f64, Logical>,
    ) -> bool {
        let Some(resize) = &resize else {
            return false;
        };

        if window != &resize.window {
            return false;
        }

        let edges = resize.data.edges;
        let horizontal = resize.horizontal;
        let vertical = resize.vertical;

        let mut changed = false;
        if edges.intersects(ResizeEdge::LEFT_RIGHT) {
            changed |= self.apply_interactive_resize(
                horizontal.as_ref(),
                Layout::SplitH,
                delta.x,
                edges.contains(ResizeEdge::LEFT),
            );
        }
        if edges.intersects(ResizeEdge::TOP_BOTTOM) {
            changed |= self.apply_interactive_resize(
                vertical.as_ref(),
                Layout::SplitV,
                delta.y,
                edges.contains(ResizeEdge::TOP),
            );
        }

        if changed {
            self.layout_with_animation_flags(workspace, false, false);
        }

        true
    }

    pub(super) fn interactive_resize_end(
        &mut self,
        resize: &mut Option<InteractiveResizeState<W>>,
        window: Option<&W::Id>,
    ) {
        let Some(ongoing) = resize.as_ref() else {
            return;
        };

        if let Some(window) = window {
            if window != &ongoing.window {
                return;
            }
        }

        *resize = None;
    }

    pub fn resize_edges_under(
        &mut self,
        workspace: NodeKey,
        pos: Point<f64, Logical>,
    ) -> Option<ResizeEdge> {
        self.resize_hit_under(workspace, pos).map(|hit| hit.edges)
    }

    pub fn resize_hit_under(
        &mut self,
        workspace: NodeKey,
        pos: Point<f64, Logical>,
    ) -> Option<ResizeHit<W::Id>> {
        let has_fullscreen_like = self.tiled_fullscreen_key(workspace).is_some()
            || self
                .focused_tile(workspace)
                .is_some_and(|tile| tile.window().is_pending_windowed_fullscreen());
        if has_fullscreen_like {
            return None;
        }

        let (leaf_key, rect) = self.closest_leaf_rect(workspace, pos)?;
        let tile = self.get_tile(leaf_key)?;
        if !tile.window().pending_sizing_mode().is_normal() {
            return None;
        }

        let border = tile.effective_border_width().unwrap_or(0.0) * 2.0;
        let threshold = super::RESIZE_EDGE_THRESHOLD.max(border);
        let gap_half = self.options(workspace).layout.gaps / 2.0;
        let edge_threshold = threshold.max(gap_half);
        let cross_threshold = threshold;

        let clamp_x = pos.x.clamp(rect.loc.x, rect.loc.x + rect.size.w);
        let clamp_y = pos.y.clamp(rect.loc.y, rect.loc.y + rect.size.h);
        let pos_within = Point::from((clamp_x - rect.loc.x, clamp_y - rect.loc.y));
        let edges =
            super::resize_edges_for_point(pos_within, rect.size, tile.effective_border_width());

        let mut best: Option<(ResizeEdge, f64)> = None;
        let mut consider_edge = |edge: ResizeEdge, dist: f64, cross_ok: bool, layout: Layout| {
            if !edges.contains(edge) || !cross_ok || dist > edge_threshold {
                return;
            }
            if !self.has_resize_target(leaf_key, edge, layout, pos) {
                return;
            }
            let score = dist / edge_threshold.max(1.0);
            if best.is_none_or(|(_, best_score)| score < best_score) {
                best = Some((edge, score));
            }
        };

        let left_dist = (pos.x - rect.loc.x).abs();
        let right_dist = (pos.x - (rect.loc.x + rect.size.w)).abs();
        let top_dist = (pos.y - rect.loc.y).abs();
        let bottom_dist = (pos.y - (rect.loc.y + rect.size.h)).abs();

        let cross_ok_y = pos.y + cross_threshold >= rect.loc.y
            && pos.y - cross_threshold <= rect.loc.y + rect.size.h;
        let cross_ok_x = pos.x + cross_threshold >= rect.loc.x
            && pos.x - cross_threshold <= rect.loc.x + rect.size.w;

        consider_edge(ResizeEdge::LEFT, left_dist, cross_ok_y, Layout::SplitH);
        consider_edge(ResizeEdge::RIGHT, right_dist, cross_ok_y, Layout::SplitH);
        consider_edge(ResizeEdge::TOP, top_dist, cross_ok_x, Layout::SplitV);
        consider_edge(ResizeEdge::BOTTOM, bottom_dist, cross_ok_x, Layout::SplitV);

        let (edge, _) = best?;

        Some(ResizeHit {
            window: tile.window().id().clone(),
            edges: edge,
            cursor: edge.cursor_icon(),
            is_floating: false,
        })
    }

    // Focus operations using ContainerTree
    pub fn activate_window(&mut self, workspace: NodeKey, window: &W::Id) -> bool {
        self.focus_window_by_id(workspace, window)
    }

    fn focus_in_direction_with_fullscreen_scope(
        &mut self,
        workspace: NodeKey,
        direction: Direction,
        allow_wrap: bool,
    ) -> bool {
        // The arena walk checks fullscreen at each ancestor exactly where sway does. That is
        // deliberately not a preselected scope: a node outside fullscreen may enter the
        // fullscreen sibling directly, while a descendant cannot climb through its owner.
        if !allow_wrap {
            self.focus_in_direction_no_wrap(workspace, direction)
        } else {
            self.focus_in_direction(workspace, direction)
        }
    }

    pub fn focus_left(&mut self, workspace: NodeKey) -> bool {
        self.focus_in_direction_with_fullscreen_scope(workspace, Direction::Left, true)
    }

    pub fn focus_left_no_wrap(&mut self, workspace: NodeKey) -> bool {
        self.focus_in_direction_with_fullscreen_scope(workspace, Direction::Left, false)
    }

    pub fn focus_right(&mut self, workspace: NodeKey) -> bool {
        self.focus_in_direction_with_fullscreen_scope(workspace, Direction::Right, true)
    }

    pub fn focus_right_no_wrap(&mut self, workspace: NodeKey) -> bool {
        self.focus_in_direction_with_fullscreen_scope(workspace, Direction::Right, false)
    }

    pub fn focus_down(&mut self, workspace: NodeKey) -> bool {
        self.focus_in_direction_with_fullscreen_scope(workspace, Direction::Down, true)
    }

    pub fn focus_down_no_wrap(&mut self, workspace: NodeKey) -> bool {
        self.focus_in_direction_with_fullscreen_scope(workspace, Direction::Down, false)
    }

    pub fn focus_up(&mut self, workspace: NodeKey) -> bool {
        self.focus_in_direction_with_fullscreen_scope(workspace, Direction::Up, true)
    }

    pub fn focus_up_no_wrap(&mut self, workspace: NodeKey) -> bool {
        self.focus_in_direction_with_fullscreen_scope(workspace, Direction::Up, false)
    }

    pub fn focus_parent(&mut self, workspace: NodeKey) -> bool {
        if let Some(fullscreen_key) = self.tiled_fullscreen_key(workspace) {
            // Focus may move inside the fullscreen subtree, including onto the container
            // that owns fullscreen, but it cannot climb through that owner. sway enforces
            // the fullscreen node as the focus boundary; treating any fullscreen presence
            // as a blanket no-op also blocked `focus parent` after a fullscreen leaf had
            // been wrapped by `split`.
            if self.command_position_is_in(fullscreen_key) {
                return self.select_parent_in(fullscreen_key);
            }
            if self.focus_parent_is_obstructed_by_fullscreen(workspace) {
                return false;
            }
        }

        self.select_parent(workspace)
    }

    /// Whether sway would reject the prospective real-container parent as fullscreen-obscured.
    ///
    /// A workspace node is not a container and remains focusable, which is why a top-level
    /// sibling outside fullscreen can still climb to the workspace. Any real parent outside
    /// the fullscreen subtree is rejected by `container_obstructing_fullscreen_container`.
    pub(super) fn focus_parent_is_obstructed_by_fullscreen(&self, workspace: NodeKey) -> bool {
        let Some(fullscreen) = self.fullscreen_key(workspace) else {
            return false;
        };
        let Some(position) = self.selected_node_key(workspace) else {
            return false;
        };
        if self.is_descendant(position, fullscreen) {
            return false;
        }
        self.parent_of(position)
            .is_some_and(|parent| parent != workspace)
    }

    /// Handle `focus parent` inside this branch's fullscreen boundary.
    ///
    /// The return value says the branch owns fullscreen, not whether focus moved: once the
    /// owner is reached, a no-op must still not be interpreted as reaching the workspace.
    pub(super) fn handle_focus_parent_in_branch_fullscreen_scope(
        &mut self,
        branch: NodeKey,
    ) -> bool {
        let Some(workspace) = self.workspace_of(branch) else {
            return false;
        };
        let Some(fullscreen) = self
            .fullscreen_key(workspace)
            .filter(|key| self.branch_root(*key) == branch)
        else {
            return false;
        };
        if !self.command_position_is_in(fullscreen) {
            return false;
        }
        let _ = self.select_parent_in(fullscreen);
        true
    }

    pub fn focus_child(&mut self, workspace: NodeKey) -> bool {
        self.select_child(workspace)
    }

    pub fn clear_selection_context(&mut self) {
        self.clear_selection();
    }

    pub(super) fn clear_workspace_selection(&mut self, workspace: NodeKey) {
        if self.workspace_is_selected(workspace) && !self.workspace_is_empty(workspace) {
            self.clear_selection();
        }
    }

    pub(super) fn root_layout_and_child_count(
        &self,
        workspace: NodeKey,
    ) -> Option<(Layout, usize)> {
        self.container_info(self.root_node_key(workspace)?)
            .map(|(layout, _rect, child_count)| (layout, child_count))
    }

    fn move_command_target(&mut self, workspace: NodeKey, direction: Direction) -> bool {
        // sway's `container_move_in_direction` opens by refusing to move a fullscreen
        // container within its workspace: one fullscreen on the workspace considers outputs
        // and nothing else, one fullscreen globally does not move at all. Neither of them
        // ever looks at the tree, so nothing below applies. The fullscreen is the tiled
        // side's to know about, which is why this guard is here and not in `move_in_branch`.
        let target = self.command_target_in(workspace);
        if self.target_is_fullscreen(target) {
            return false;
        }
        self.move_in_branch(workspace, direction)
    }

    /// Move whatever a command in this branch is aimed at, and arrange if it moved.
    pub(super) fn move_in_branch(&mut self, branch: NodeKey, direction: Direction) -> bool {
        let Some(workspace) = self.workspace_of(branch) else {
            return false;
        };
        let target = self.command_target_in(branch);
        self.mutate_tree(workspace, |tree| {
            tree.move_target_in_direction(direction, target)
        })
    }

    fn target_is_fullscreen(&self, target: NodeKey) -> bool {
        let Some(workspace) = self.workspace_of(target) else {
            return false;
        };
        if target == workspace {
            return false;
        }
        // `container_move_in_direction` refuses the container whose fullscreen mode is set,
        // not every descendant inside a fullscreen container. Descendants may rearrange
        // within that boundary; `move_node` stops them if climbing would cross the owner.
        self.tiled_fullscreen_key(workspace) == Some(target)
    }

    // Move operations using ContainerTree
    pub fn move_left(&mut self, workspace: NodeKey) -> bool {
        self.move_command_target(workspace, Direction::Left)
    }

    pub fn move_right(&mut self, workspace: NodeKey) -> bool {
        self.move_command_target(workspace, Direction::Right)
    }

    pub fn move_down(&mut self, workspace: NodeKey) -> bool {
        self.move_command_target(workspace, Direction::Down)
    }

    pub fn move_up(&mut self, workspace: NodeKey) -> bool {
        self.move_command_target(workspace, Direction::Up)
    }

    // Container operations (replacing column operations)
    pub fn consume_into_column(&mut self, workspace: NodeKey) {
        // In i3 model: create vertical split
        self.split_in_branch(workspace, Layout::SplitV);
    }

    pub fn expel_from_column(&mut self, workspace: NodeKey) {
        // In i3 model: create horizontal split
        self.split_in_branch(workspace, Layout::SplitH);
    }

    /// Split focused window horizontally (i3-style)
    pub fn split_horizontal(&mut self, workspace: NodeKey) {
        self.split_in_branch(workspace, Layout::SplitH);
    }

    /// Split focused window vertically (i3-style)
    pub fn split_vertical(&mut self, workspace: NodeKey) {
        self.split_in_branch(workspace, Layout::SplitV);
    }

    /// Split workspace root like workspace root split.
    pub fn split_workspace_horizontal(&mut self, workspace: NodeKey) {
        self.split_workspace(workspace, Layout::SplitH);
    }

    /// Split workspace root like workspace root split.
    pub fn split_workspace_vertical(&mut self, workspace: NodeKey) {
        self.split_workspace(workspace, Layout::SplitV);
    }

    /// `split` with the workspace itself selected. The rule lives on the tree; this is the
    /// space's relayout contract around it.
    fn split_workspace(&mut self, workspace: NodeKey, layout: Layout) {
        self.mutate_tree(workspace, |tree| {
            tree.split_workspace_container(workspace, layout)
        });
    }

    /// Set layout mode for focused container
    pub fn set_layout_mode(&mut self, workspace: NodeKey, layout: Layout) {
        self.set_layout_in_branch(workspace, layout);
    }

    pub fn split_none(&mut self, workspace: NodeKey) {
        self.unsplit_in_branch(workspace);
    }

    /// `layout` with the workspace itself selected.
    ///
    /// Measured against sway 1.11: this never builds a container. The workspace node carries
    /// the orientation itself, so an empty workspace and a lone window are the same case.
    pub fn set_workspace_layout_mode(&mut self, workspace: NodeKey, layout: Layout) -> bool {
        self.mutate_tree(workspace, |tree| {
            tree.set_root_layout_from_command(workspace, layout)
        })
    }

    /// The layout of the parent of whatever a command is aimed at.
    ///
    /// sway's `container_parent_layout(config->handler_context.container)`. `None` when the
    /// workspace itself is what is selected: sway has no container to ask there, because a
    /// workspace is not one.
    pub fn command_target_parent_layout(&self, workspace: NodeKey) -> Option<Layout> {
        let key = self.selected_node_key(workspace)?;
        if Some(key) == self.root_node_key(workspace) {
            return None;
        }
        self.parent_layout(key)
    }

    /// Toggle between horizontal and vertical split for the focused container.
    pub fn toggle_split_layout(&mut self, workspace: NodeKey) {
        self.toggle_split_in_branch(workspace);
    }

    pub fn toggle_workspace_split_layout(&mut self, workspace: NodeKey) {
        let next = self.toggled_split_layout(
            workspace,
            self.workspace_layout(workspace),
            self.root_node_key(workspace),
        );
        self.set_workspace_layout_mode(workspace, next);
    }

    /// Cycle focused container layout in sway-style order.
    pub fn toggle_layout_all(&mut self, workspace: NodeKey) {
        self.toggle_layout_all_in_branch(workspace);
    }

    pub fn toggle_workspace_layout_all(&mut self, workspace: NodeKey) {
        let next = self.workspace_layout(workspace).next_in_cycle();
        self.set_workspace_layout_mode(workspace, next);
    }

    pub fn set_default_layout(&mut self, workspace: NodeKey) {
        self.set_default_layout_in_branch(workspace);
    }

    pub fn set_workspace_default_layout(&mut self, workspace: NodeKey) {
        self.mutate_tree(workspace, |tree| {
            let root = workspace;
            tree.set_default_layout_for_target(root)
        });
    }

    pub(super) fn toggle_layout_cycle(&mut self, workspace: NodeKey, cycle: &[LayoutCycleEntry]) {
        self.toggle_layout_cycle_in_branch(workspace, cycle);
    }

    pub(super) fn toggle_workspace_layout_cycle(
        &mut self,
        workspace: NodeKey,
        cycle: &[LayoutCycleEntry],
    ) {
        self.mutate_tree(workspace, |tree| {
            let root = workspace;
            tree.toggle_layout_cycle_for_target(root, cycle)
        });
    }

    /// Set the width of the currently focused root-level column
    pub fn set_column_width(&mut self, workspace: NodeKey, change: SizeChange) {
        let Some(idx) = self.focused_root_index(workspace) else {
            return;
        };
        let Some(root_key) = self.root_node_key(workspace) else {
            return;
        };

        let Some((layout, rect, child_count)) = self.container_info(root_key) else {
            return;
        };
        if layout != Layout::SplitH || child_count == 0 {
            return;
        }

        let available_width = self.available_span(workspace, rect.size.w, child_count);
        if available_width <= 0.0 {
            return;
        }

        let current_percent = self.child_percent(root_key, idx).unwrap_or(1.0);
        let new_percent =
            percent_from_size_change(current_percent, available_width, || rect.size.w, change);

        if self.set_child_percent(root_key, idx, Layout::SplitH, new_percent) {
            self.layout(workspace);
        }
    }
    pub fn reset_window_height(&mut self, workspace: NodeKey, window: Option<&W::Id>) {
        let Some(key) = self.window_target(workspace, window) else {
            return;
        };

        let Some((parent_key, _, _, _child_count, _rect)) =
            self.window_container_metrics(key, Layout::SplitV)
        else {
            return;
        };

        if self
            .container_info(parent_key)
            .is_some_and(|(layout, _, _)| layout == Layout::SplitV)
        {
            self.recalculate_child_percents(parent_key);
            self.layout(workspace);
        }
    }

    /// Toggle fullscreen state for a window
    pub fn toggle_fullscreen(&mut self, workspace: NodeKey, window: &W) {
        let currently = self.is_fullscreen(workspace, window);
        let _ = self.set_fullscreen(workspace, window.id(), !currently);
    }
    pub fn toggle_width(&mut self, workspace: NodeKey, forwards: bool) {
        let Some(idx) = self.focused_root_index(workspace) else {
            return;
        };
        let Some(root_key) = self.root_node_key(workspace) else {
            return;
        };

        let Some((layout, rect, child_count)) = self.container_info(root_key) else {
            return;
        };
        if layout != Layout::SplitH || child_count == 0 {
            return;
        }

        let available = self.available_span(workspace, rect.size.w, child_count);
        if available <= 0.0 {
            return;
        }

        let current_percent = self.child_percent(root_key, idx).unwrap_or(1.0);
        let presets = &self.options(workspace).layout.preset_column_widths;

        if let Some(percent) = self.cycle_presets(available, current_percent, presets, forwards) {
            if self.set_child_percent(root_key, idx, Layout::SplitH, percent) {
                self.layout(workspace);
            }
        }
    }

    #[cfg(test)]
    pub fn active_root_index(&self, workspace: NodeKey) -> usize {
        self.focused_root_index(workspace).unwrap_or(0)
    }

    const DROP_LAYOUT_BORDER: f64 = 30.0;
    const DROP_CENTER_RATIO: f64 = 0.3;

    fn closest_edge(rect: Rectangle<f64, Logical>, pos: Point<f64, Logical>) -> (Direction, f64) {
        let left = (pos.x - rect.loc.x).abs();
        let right = (rect.loc.x + rect.size.w - pos.x).abs();
        let top = (pos.y - rect.loc.y).abs();
        let bottom = (rect.loc.y + rect.size.h - pos.y).abs();

        let mut dir = Direction::Left;
        let mut min = left;

        if right < min {
            min = right;
            dir = Direction::Right;
        }
        if top < min {
            min = top;
            dir = Direction::Up;
        }
        if bottom < min {
            min = bottom;
            dir = Direction::Down;
        }

        (dir, min)
    }

    fn leaf_rect_for_key(
        &self,
        workspace: NodeKey,
        key: NodeKey,
    ) -> Option<Rectangle<f64, Logical>> {
        let scale = Scale::from(self.scale(workspace));
        let info = self
            .display_layouts(workspace)
            .find(|info| info.key == key)?;
        let tile = self.get_tile(info.key)?;
        let mut tile_pos = info.rect.loc + tile.render_offset();
        tile_pos = tile_pos.to_physical_precise_round(scale).to_logical(scale);
        Some(Rectangle::new(tile_pos, tile.tile_size()))
    }

    /// The leaf under `pos`, or the nearest one: its stable identity and on-screen rect.
    fn closest_leaf_rect(&self, workspace: NodeKey, pos: Point<f64, Logical>) -> Option<LeafHit> {
        let scale = Scale::from(self.scale(workspace));
        let fullscreen = self.fullscreen_render_state(workspace);

        let mut nearest: Option<(LeafHit, f64)> = None;

        for info in self.display_layouts(workspace) {
            if let Some(tile) = self.get_tile(info.key) {
                let in_fullscreen_container = fullscreen
                    .container
                    .is_some_and(|scope| self.is_descendant(info.key, scope));
                let state = fullscreen.tile_state(
                    tile.window().id(),
                    info.visible,
                    in_fullscreen_container,
                );
                if !state.visible {
                    continue;
                }

                let base_pos = if state.client_fullscreen {
                    Point::from((0.0, 0.0))
                } else {
                    info.rect.loc
                };
                let mut tile_pos = base_pos + tile.render_offset();
                tile_pos = tile_pos.to_physical_precise_round(scale).to_logical(scale);
                let tile_rect = Rectangle::new(tile_pos, tile.tile_size());

                if tile_rect.contains(pos) {
                    return Some((info.key, tile_rect));
                }

                let dx = if pos.x < tile_rect.loc.x {
                    tile_rect.loc.x - pos.x
                } else if pos.x > tile_rect.loc.x + tile_rect.size.w {
                    pos.x - (tile_rect.loc.x + tile_rect.size.w)
                } else {
                    0.0
                };
                let dy = if pos.y < tile_rect.loc.y {
                    tile_rect.loc.y - pos.y
                } else if pos.y > tile_rect.loc.y + tile_rect.size.h {
                    pos.y - (tile_rect.loc.y + tile_rect.size.h)
                } else {
                    0.0
                };
                let dist2 = dx * dx + dy * dy;

                let replace = nearest.as_ref().is_none_or(|(_, best)| dist2 < *best);
                if replace {
                    nearest = Some(((info.key, tile_rect), dist2));
                }
            }
        }

        nearest.map(|(hit, _)| hit)
    }

    fn indicator_rect(
        rect: Rectangle<f64, Logical>,
        direction: Direction,
        thickness: f64,
    ) -> Rectangle<f64, Logical> {
        let thickness = thickness.max(1.0);
        match direction {
            Direction::Left => Rectangle::new(
                rect.loc,
                Size::from((thickness.min(rect.size.w), rect.size.h)),
            ),
            Direction::Right => Rectangle::new(
                Point::from((
                    rect.loc.x + rect.size.w - thickness.min(rect.size.w),
                    rect.loc.y,
                )),
                Size::from((thickness.min(rect.size.w), rect.size.h)),
            ),
            Direction::Up => Rectangle::new(
                rect.loc,
                Size::from((rect.size.w, thickness.min(rect.size.h))),
            ),
            Direction::Down => Rectangle::new(
                Point::from((
                    rect.loc.x,
                    rect.loc.y + rect.size.h - thickness.min(rect.size.h),
                )),
                Size::from((rect.size.w, thickness.min(rect.size.h))),
            ),
        }
    }

    fn inset_rect(rect: Rectangle<f64, Logical>, inset: f64) -> Rectangle<f64, Logical> {
        let inset = inset.min(rect.size.w / 2.0).min(rect.size.h / 2.0).max(0.0);
        Rectangle::new(
            Point::from((rect.loc.x + inset, rect.loc.y + inset)),
            Size::from((rect.size.w - 2.0 * inset, rect.size.h - 2.0 * inset)),
        )
    }

    /// Determine insert position from pointer location
    pub(super) fn insert_position(
        &self,
        workspace: NodeKey,
        pos: Point<f64, Logical>,
    ) -> InsertPosition {
        if self.workspace_is_empty(workspace) {
            return InsertPosition::RootStart;
        }

        let layout_area = self.layout_area(workspace);
        if pos.y < layout_area.loc.y + Self::DROP_LAYOUT_BORDER {
            return InsertPosition::SplitRoot {
                direction: Direction::Up,
                indicator: SplitIndicator::LayoutBorder,
            };
        }
        if pos.y > layout_area.loc.y + layout_area.size.h - Self::DROP_LAYOUT_BORDER {
            return InsertPosition::SplitRoot {
                direction: Direction::Down,
                indicator: SplitIndicator::LayoutBorder,
            };
        }

        let Some((leaf_key, rect)) = self.closest_leaf_rect(workspace, pos) else {
            return InsertPosition::RootStart;
        };

        let parent_layout = self.parent_layout(leaf_key).unwrap_or(Layout::SplitH);

        if matches!(parent_layout, Layout::SplitH | Layout::Tabbed) {
            if pos.y < rect.loc.y + Self::DROP_LAYOUT_BORDER {
                return InsertPosition::Split {
                    target: leaf_key,
                    direction: Direction::Up,
                    indicator: SplitIndicator::LayoutBorder,
                };
            }
            if pos.y > rect.loc.y + rect.size.h - Self::DROP_LAYOUT_BORDER {
                return InsertPosition::Split {
                    target: leaf_key,
                    direction: Direction::Down,
                    indicator: SplitIndicator::LayoutBorder,
                };
            }
        } else if matches!(parent_layout, Layout::SplitV | Layout::Stacked) {
            if pos.x < rect.loc.x + Self::DROP_LAYOUT_BORDER {
                return InsertPosition::Split {
                    target: leaf_key,
                    direction: Direction::Left,
                    indicator: SplitIndicator::LayoutBorder,
                };
            }
            if pos.x > rect.loc.x + rect.size.w - Self::DROP_LAYOUT_BORDER {
                return InsertPosition::Split {
                    target: leaf_key,
                    direction: Direction::Right,
                    indicator: SplitIndicator::LayoutBorder,
                };
            }
        }

        let (direction, dist) = Self::closest_edge(rect, pos);
        let thickness = f64::min(rect.size.w, rect.size.h) * Self::DROP_CENTER_RATIO;
        if dist > thickness {
            InsertPosition::Swap {
                target: leaf_key,
                direction,
            }
        } else {
            InsertPosition::Split {
                target: leaf_key,
                direction,
                indicator: SplitIndicator::Center,
            }
        }
    }

    /// Get hint area for insertion position
    pub(super) fn insert_hint_area(
        &self,
        workspace: NodeKey,
        position: &InsertPosition,
    ) -> Option<Rectangle<f64, Logical>> {
        match position {
            InsertPosition::RootStart => Some(self.layout_area(workspace)),
            InsertPosition::Swap { target, .. } => {
                let rect = self.leaf_rect_for_key(workspace, *target)?;
                let thickness = f64::min(rect.size.w, rect.size.h) * Self::DROP_CENTER_RATIO;
                Some(Self::inset_rect(rect, thickness))
            }
            InsertPosition::Split {
                target,
                direction,
                indicator,
            } => {
                let rect = self.leaf_rect_for_key(workspace, *target)?;
                let thickness = match indicator {
                    SplitIndicator::LayoutBorder => Self::DROP_LAYOUT_BORDER,
                    SplitIndicator::Center => {
                        f64::min(rect.size.w, rect.size.h) * Self::DROP_CENTER_RATIO
                    }
                };
                Some(Self::indicator_rect(rect, *direction, thickness))
            }
            InsertPosition::SplitRoot {
                direction,
                indicator,
            } => {
                let rect = self.layout_area(workspace);
                let thickness = match indicator {
                    SplitIndicator::LayoutBorder => Self::DROP_LAYOUT_BORDER,
                    SplitIndicator::Center => {
                        f64::min(rect.size.w, rect.size.h) * Self::DROP_CENTER_RATIO
                    }
                };
                Some(Self::indicator_rect(rect, *direction, thickness))
            }
            InsertPosition::Floating => None,
        }
    }

    // Window queries
    fn tab_bar_hit(
        &self,
        workspace: NodeKey,
        pos: Point<f64, Logical>,
    ) -> Option<(&W, super::HitType)> {
        if self.render_fullscreen_window(workspace).is_some() {
            return None;
        }
        self.branch_tab_bar_hit(workspace, pos, 0)
    }

    /// Which window a click on one of this branch's tab bars lands on.
    ///
    /// `pad` widens every bar by that many physical pixels before testing. A floating group's
    /// bar is the top edge of a window the user is going to drag, so its edges are forgiving;
    /// the tiled side's are not, because next to them is another window.
    ///
    /// The cached tab widths are the ones the bar was last drawn with. Measuring the text
    /// again here would answer a slightly different question than the one on screen.
    pub(super) fn branch_tab_bar_hit(
        &self,
        branch: NodeKey,
        pos: Point<f64, Logical>,
        pad: i32,
    ) -> Option<(&W, super::HitType)> {
        let workspace = self.workspace_of(branch)?;
        if self.options(workspace).layout.tab_bar.off {
            return None;
        }

        let gap = self.branch_gap(branch);
        let fullscreen_container = (branch == workspace)
            .then(|| self.render_fullscreen_container(workspace))
            .flatten();
        let cache = self.render.tab_bar_cache.borrow();
        for mut info in self.tab_bar_layouts_in_branch(branch) {
            if fullscreen_container.is_some_and(|scope| !self.is_descendant(info.key, scope)) {
                continue;
            }
            // A branch's outermost bar sits on the branch's edge, and the gap is outside it.
            if gap > 0.0 && info.key == branch {
                info.rect.loc.x -= gap;
                info.rect.loc.y -= gap;
                info.rect.size.w = (info.rect.size.w + gap * 2.0).max(0.0);
            }
            let cached_widths = cache
                .get(&info.key)
                .map(|entry| entry.tab_widths_px.as_slice());
            let Some(tab_idx) =
                tab_bar_hit_index(&info, pos, self.scale(workspace), cached_widths, pad)
            else {
                continue;
            };

            if let Some(window) = self.window_for_tab(info.key, tab_idx) {
                return Some((
                    window,
                    super::HitType::Activate {
                        is_tab_indicator: true,
                    },
                ));
            }
        }

        None
    }

    pub fn window_under(
        &self,
        workspace: NodeKey,
        pos: Point<f64, Logical>,
    ) -> Option<(&W, super::HitType)> {
        let scale = Scale::from(self.scale(workspace));
        let fullscreen = self.fullscreen_render_state(workspace);

        if let Some(hit) = self.tab_bar_hit(workspace, pos) {
            return Some(hit);
        }

        let render_layouts: Vec<&LeafLayoutInfo> = self.display_layouts(workspace).collect();
        for info in render_layouts.iter().rev() {
            // Use O(1) key lookup instead of O(depth) path lookup.
            if let Some(tile) = self.get_tile(info.key) {
                let in_fullscreen_container = fullscreen
                    .container
                    .is_some_and(|scope| self.is_descendant(info.key, scope));
                let state = fullscreen.tile_state(
                    tile.window().id(),
                    info.visible,
                    in_fullscreen_container,
                );
                if !state.visible {
                    continue;
                }

                // Fullscreen-like tiles are rendered relative to the workspace origin.
                let base_pos = if state.client_fullscreen {
                    Point::from((0.0, 0.0))
                } else {
                    info.rect.loc
                };
                let mut tile_pos = base_pos + tile.render_offset();
                tile_pos = tile_pos.to_physical_precise_round(scale).to_logical(scale);

                if let Some(hit) = super::HitType::hit_tile(tile, tile_pos, pos) {
                    return Some(hit);
                }
            }
        }

        None
    }

    pub fn window_loc(&self, workspace: NodeKey, window: &W) -> Option<Point<f64, Logical>> {
        let key = self.tiled_window_key(workspace, window.id())?;
        let info = self
            .display_layouts(workspace)
            .find(|layout| layout.key == key)?;
        let tile = self.get_tile(key)?;
        let scale = Scale::from(self.scale(workspace));

        let mut tile_pos = info.rect.loc + tile.render_offset();
        tile_pos = tile_pos.to_physical_precise_round(scale).to_logical(scale);

        Some(tile_pos + tile.window_loc())
    }

    pub fn window_size(&self, workspace: NodeKey, window: &W) -> Option<Size<f64, Logical>> {
        let key = self.tiled_window_key(workspace, window.id())?;
        let tile = self.get_tile(key)?;
        Some(tile.window_size())
    }

    pub fn is_fullscreen(&self, workspace: NodeKey, window: &W) -> bool {
        self.tiled_window_key(workspace, window.id()).is_some()
            && self.window_owns_fullscreen(workspace, window.id())
    }

    pub fn has_fullscreen_window(&self, workspace: NodeKey) -> bool {
        self.tiled_fullscreen_key(workspace).is_some()
    }

    /// Set the display mode for the focused container
    pub fn set_column_display(&mut self, workspace: NodeKey, display: ColumnDisplay) {
        let layout = match display {
            ColumnDisplay::Normal => Layout::SplitV,
            ColumnDisplay::Tabbed => Layout::Tabbed,
        };

        self.set_layout_in_branch(workspace, layout);
    }

    /// Toggle between tabbed and normal (split) layout for focused container
    pub fn toggle_column_tabbed_display(&mut self, workspace: NodeKey) {
        let current = self.selection_layout_in(workspace);
        let target = match current {
            Some(Layout::Tabbed) => Layout::SplitV,
            _ => Layout::Tabbed,
        };

        self.set_layout_in_branch(workspace, target);
    }

    // Additional methods needed by workspace.rs
    pub fn tiles_mut(&mut self, workspace: NodeKey) -> impl Iterator<Item = &mut Tile<W>> + '_ {
        let root = workspace;
        self.tiles_in_branch_mut(root).into_iter()
    }

    pub fn tiles_with_render_positions(
        &self,
        workspace: NodeKey,
    ) -> impl Iterator<Item = (&Tile<W>, Point<f64, Logical>, bool)> + '_ {
        let scale = Scale::from(self.scale(workspace));
        let fullscreen_container = self.render_fullscreen_container(workspace);
        self.display_layouts(workspace).filter_map(move |info| {
            // Use O(1) key lookup instead of O(depth) path lookup.
            let tile = self.get_tile(info.key)?;
            let pos = info.rect.loc + tile.render_offset();
            let pos = pos.to_physical_precise_round(scale).to_logical(scale);
            let visible = info.visible
                && fullscreen_container.is_none_or(|scope| self.is_descendant(info.key, scope));
            Some((tile, pos, visible))
        })
    }

    pub fn tiles_with_render_positions_mut(
        &mut self,
        workspace: NodeKey,
        round: bool,
    ) -> impl Iterator<Item = (&mut Tile<W>, Point<f64, Logical>)> + '_ {
        let scale = Scale::from(self.scale(workspace));
        let layouts: Vec<LeafLayoutInfo> = self.display_layouts(workspace).cloned().collect();
        let keys: Vec<NodeKey> = layouts.iter().map(|info| info.key).collect();
        let locs: Vec<Point<f64, Logical>> = layouts.iter().map(|info| info.rect.loc).collect();
        self.tiles_mut_for_keys(&keys)
            .into_iter()
            .map(move |(idx, tile)| {
                let mut pos = locs[idx] + tile.render_offset();
                if round {
                    pos = pos.to_physical_precise_round(scale).to_logical(scale);
                }
                (tile, pos)
            })
    }

    pub fn tiles_with_ipc_layouts(
        &self,
        workspace: NodeKey,
    ) -> impl Iterator<Item = (&Tile<W>, tiri_ipc::WindowLayout)> + '_ {
        let scale = Scale::from(self.scale(workspace));
        let legacy_positions = self.legacy_tiling_positions(workspace);

        self.display_layouts(workspace).filter_map(move |info| {
            let tile = self.get_tile(info.key)?;
            let mut layout = tile.ipc_layout_template();
            let tile_size = tile.tile_size();
            layout.tile_size = (tile_size.w, tile_size.h);
            let window_size = tile.window_size().to_i32_round();
            layout.window_size = (window_size.w, window_size.h);
            let mut pos = info.rect.loc + tile.render_offset();
            pos = pos.to_physical_precise_round(scale).to_logical(scale);
            layout.tile_pos_in_workspace_view = Some((pos.x, pos.y));
            let window_offset = tile.window_loc();
            layout.window_offset_in_tile = (window_offset.x, window_offset.y);
            layout.pos_in_tiling_layout = self
                .branch_relative_path(info.key)
                .and_then(|path| legacy_positions.get(&path).copied());
            Some((tile, layout))
        })
    }

    fn legacy_tiling_positions(&self, workspace: NodeKey) -> HashMap<Vec<usize>, (usize, usize)> {
        let mut positions = HashMap::new();

        if self.root_children_len(workspace) == 0 {
            return positions;
        }

        if self.root_container(workspace).is_none() {
            positions.insert(Vec::new(), (1, 1));
            return positions;
        }

        for root_idx in 0..self.root_children_len(workspace) {
            for (leaf_idx, path) in self
                .leaf_paths_under(workspace, &[root_idx])
                .into_iter()
                .enumerate()
            {
                positions.insert(path, (root_idx + 1, leaf_idx + 1));
            }
        }

        positions
    }

    pub fn are_transitions_ongoing(&self, workspace: NodeKey) -> bool {
        self.tiles(workspace)
            .any(|tile| tile.are_transitions_ongoing())
    }

    pub fn update_shaders(&mut self, workspace: NodeKey) {
        for tile in self.tiles_mut(workspace) {
            tile.update_shaders();
        }
    }

    pub fn active_window(&self, workspace: NodeKey) -> Option<&W> {
        self.focused_window_in_branch(workspace)
    }

    /// Whether one workspace's tiled side holds no window.
    pub fn is_empty(&self, workspace: NodeKey) -> bool {
        self.first_leaf_key(workspace).is_none()
    }

    pub(super) fn add_tile(
        &mut self,
        workspace: NodeKey,
        root_index: Option<usize>,
        tile: IncomingView<W>,
        activate: bool,
    ) {
        let id = self
            .incoming_tile(&tile)
            .expect("an insertion must own a valid incoming view")
            .window()
            .id()
            .clone();
        if let Some(index) = root_index {
            self.insert_leaf_at(workspace, index, tile, activate);
        } else {
            self.insert_window_with_focus(workspace, tile, activate);
        }
        self.sync_fullscreen_window(workspace);
        self.layout_after_tiled_insert(workspace, &id);
    }

    pub(super) fn add_tile_right_of(
        &mut self,
        workspace: NodeKey,
        next_to: &W::Id,
        tile: IncomingView<W>,
        activate: bool,
    ) {
        let id = self
            .incoming_tile(&tile)
            .expect("an insertion must own a valid incoming view")
            .window()
            .id()
            .clone();
        self.insert_leaf_after(workspace, next_to, tile, activate);
        self.sync_fullscreen_window(workspace);
        self.layout_after_tiled_insert(workspace, &id);
    }

    pub(super) fn replace_tiling_tile(
        &mut self,
        key: NodeKey,
        tile: IncomingView<W>,
    ) -> Result<IncomingView<W>, IncomingView<W>> {
        if !self.is_tiling_leaf(key) {
            return Err(tile);
        }
        self.replace_leaf(key, tile)
    }

    pub(super) fn is_tiling_leaf(&self, key: NodeKey) -> bool {
        let Some(workspace) = self.workspace_of(key) else {
            return false;
        };
        self.is_leaf(key) && self.branch_root(key) == workspace
    }

    pub(super) fn insert_tile_with_parent_info(
        &mut self,
        workspace: NodeKey,
        info: &super::container::InsertParentInfo,
        tile: IncomingView<W>,
        activate: bool,
    ) -> bool {
        let root = workspace;
        if self.insert_leaf_with_parent_info(root, info, tile, activate) {
            self.sync_fullscreen_window(workspace);
            self.layout(workspace);
            return true;
        }

        false
    }

    /// Split-insert next to the live tiling leaf selected by the drag-and-drop hit result.
    pub(super) fn insert_tile_split(
        &mut self,
        target: NodeKey,
        direction: Direction,
        tile: IncomingView<W>,
        activate: bool,
    ) -> bool {
        let Some(workspace) = self.workspace_of(target) else {
            return false;
        };
        if !self.is_tiling_leaf(target) {
            return false;
        }
        if self.insert_leaf_split(target, direction, tile, activate) {
            self.sync_fullscreen_window(workspace);
            self.layout(workspace);
            return true;
        }

        false
    }

    pub(super) fn insert_tile_split_root(
        &mut self,
        workspace: NodeKey,
        direction: Direction,
        tile: IncomingView<W>,
        activate: bool,
    ) -> bool {
        if self.insert_leaf_split_root(workspace, direction, tile, activate) {
            self.sync_fullscreen_window(workspace);
            self.layout(workspace);
            return true;
        }

        false
    }

    pub fn active_tile_visual_rectangle(
        &self,
        workspace: NodeKey,
    ) -> Option<Rectangle<f64, Logical>> {
        let focused_key = self.focused_key(workspace);
        focused_key
            .and_then(|key| self.leaf_layout(key))
            .and_then(|info| {
                let mut rect = info.rect;
                let tile = self.get_tile(info.key)?;
                rect.loc += tile.render_offset();
                Some(rect)
            })
    }

    /// Get mutable reference to the currently focused tile
    pub fn active_tile_mut(&mut self, workspace: NodeKey) -> Option<&mut Tile<W>> {
        let key = self.focus_inactive_view_in_branch(workspace)?;
        self.get_tile_mut(key)
    }

    pub fn add_root_tiling_subtree(
        &mut self,
        workspace: NodeKey,
        root_idx: Option<usize>,
        subtree: RootTilingSubtree,
        activate: bool,
    ) {
        let idx = root_idx.unwrap_or_else(|| self.root_children_len(workspace));
        self.insert_subtree_at_root(workspace, idx, subtree.into_subtree(), activate);
        self.sync_fullscreen_window(workspace);
        self.layout(workspace);
    }

    pub(super) fn park_tile(
        &mut self,
        workspace: NodeKey,
        window: &W::Id,
        transaction: Transaction,
    ) -> ParkedTile {
        self.set_pending_transaction(transaction.clone());
        let node = self
            .park_window(workspace, window)
            .expect("attempted to remove missing window");

        ParkedTile {
            node,
            is_floating: false,
        }
    }
    pub fn remove_active_root_tiling_subtree(
        &mut self,
        workspace: NodeKey,
    ) -> Option<RootTilingSubtree> {
        let idx = self.focused_root_index(workspace)?;
        let subtree = self.take_root_child_subtree(workspace, idx)?;
        let subtree = RootTilingSubtree::from_subtree(subtree);

        self.layout(workspace);
        Some(subtree)
    }

    /// Remove the leaf or explicitly selected parent addressed by `move container`.
    pub fn remove_active_tiling_subtree(
        &mut self,
        workspace: NodeKey,
    ) -> Option<RootTilingSubtree> {
        let subtree = self.take_command_target_subtree(workspace)?;
        let subtree = RootTilingSubtree::from_subtree(subtree);

        self.layout(workspace);
        Some(subtree)
    }

    pub fn new_window_size(
        &self,
        workspace: NodeKey,
        _width: Option<PresetSize>,
        _height: Option<PresetSize>,
        rules: &ResolvedWindowRules,
    ) -> Size<i32, Logical> {
        let Some(preview) = self.preview_new_leaf_geometry(workspace) else {
            return Size::from((800, 600));
        };

        let mut size = preview.rect.size;
        let mut border_config = self
            .options(workspace)
            .layout
            .border
            .merged_with(&rules.border);
        border_config.width =
            round_logical_in_physical_max1(self.scale(workspace), border_config.width);

        if !border_config.off {
            let width = border_config.width * 2.0;
            size.w = f64::max(1.0, size.w - width);
            size.h = f64::max(1.0, size.h - width);
        }
        if preview.tab_bar_offset > 0.0 {
            size.h = f64::max(1.0, size.h - preview.tab_bar_offset);
        }

        size.to_i32_floor()
    }

    pub fn new_window_toplevel_bounds(&self, _rules: &ResolvedWindowRules) -> Size<i32, Logical> {
        Size::from((800, 600))
    }

    pub fn focus_root_container_first(&mut self, workspace: NodeKey) {
        self.focus_root_child(workspace, 0);
    }

    pub fn focus_first_leaf(&mut self, workspace: NodeKey) {
        let _ =
            self.focus_leaf_in_root_child(workspace, 0, 1) || self.focus_root_child(workspace, 0);
    }

    pub fn focus_root_container_last(&mut self, workspace: NodeKey) {
        let len = self.root_children_len(workspace);
        if len > 0 {
            self.focus_root_child(workspace, len - 1);
        }
    }

    /// Root containers are 1-based to match user-facing commands.
    pub fn focus_root_container(&mut self, workspace: NodeKey, idx: usize) {
        if idx == 0 {
            return;
        }
        self.focus_root_child(workspace, idx - 1);
    }

    /// Leaves inside the current root container are 1-based.
    pub fn focus_leaf_in_root_container(&mut self, workspace: NodeKey, index: u8) {
        if index == 0 {
            return;
        }
        let root_idx = match self.focused_root_index(workspace) {
            Some(idx) => idx,
            None => return,
        };
        self.focus_leaf_in_root_child(workspace, root_idx, index as usize);
    }

    pub fn focus_column_first(&mut self, workspace: NodeKey) {
        self.focus_root_container_first(workspace);
    }

    pub fn focus_column_last(&mut self, workspace: NodeKey) {
        self.focus_root_container_last(workspace);
    }

    pub fn focus_column(&mut self, workspace: NodeKey, idx: usize) {
        self.focus_root_container(workspace, idx);
    }

    pub fn focus_window_in_column(&mut self, workspace: NodeKey, index: u8) {
        self.focus_leaf_in_root_container(workspace, index);
    }

    pub fn focus_down_or_left(&mut self, workspace: NodeKey) {
        let _ = self.focus_in_direction(workspace, Direction::Down)
            || self.focus_in_direction(workspace, Direction::Left);
    }

    pub fn focus_down_or_right(&mut self, workspace: NodeKey) {
        let _ = self.focus_in_direction(workspace, Direction::Down)
            || self.focus_in_direction(workspace, Direction::Right);
    }

    pub fn focus_up_or_left(&mut self, workspace: NodeKey) {
        let _ = self.focus_in_direction(workspace, Direction::Up)
            || self.focus_in_direction(workspace, Direction::Left);
    }

    pub fn focus_up_or_right(&mut self, workspace: NodeKey) {
        let _ = self.focus_in_direction(workspace, Direction::Up)
            || self.focus_in_direction(workspace, Direction::Right);
    }

    pub fn focus_top(&mut self, workspace: NodeKey) {
        self.focus_first_leaf_in_focused_root_child(workspace);
    }

    pub fn focus_bottom(&mut self, workspace: NodeKey) {
        self.focus_last_leaf_in_focused_root_child(workspace);
    }

    fn move_root_child_with_layout(
        &mut self,
        workspace: NodeKey,
        current: usize,
        target: usize,
    ) -> bool {
        if current == target {
            return false;
        }
        if target >= self.root_children_len(workspace) {
            return false;
        }
        self.mutate_tree(workspace, |tree| {
            tree.move_root_child(workspace, current, target)
        })
    }

    pub fn move_root_container_to_first(&mut self, workspace: NodeKey) {
        if let Some(idx) = self.focused_root_index(workspace) {
            self.move_root_child_with_layout(workspace, idx, 0);
        }
    }

    pub fn move_root_container_to_last(&mut self, workspace: NodeKey) {
        let len = self.root_children_len(workspace);
        if len == 0 {
            return;
        }
        if let Some(idx) = self.focused_root_index(workspace) {
            self.move_root_child_with_layout(workspace, idx, len - 1);
        }
    }

    pub fn move_root_container_to_index(&mut self, workspace: NodeKey, idx: usize) {
        if idx == 0 {
            return;
        }
        let target = idx - 1;
        if let Some(current) = self.focused_root_index(workspace) {
            if current == target {
                return;
            }
            self.move_root_child_with_layout(workspace, current, target);
        }
    }

    pub fn move_column_to_first(&mut self, workspace: NodeKey) {
        self.move_root_container_to_first(workspace);
    }

    pub fn move_column_to_last(&mut self, workspace: NodeKey) {
        self.move_root_container_to_last(workspace);
    }

    pub fn move_column_to_index(&mut self, workspace: NodeKey, idx: usize) {
        self.move_root_container_to_index(workspace, idx);
    }

    fn consume_or_expel_window(
        &mut self,
        workspace: NodeKey,
        window: Option<&W::Id>,
        direction: Direction,
    ) {
        if let Some(id) = window {
            self.focus_window_by_id(workspace, id);
        }

        if !self.move_command_target(workspace, direction) {
            // Scoped to the tiled branch, like the move above it. One seat spans the whole
            // workspace, so the focused node can be a floating one this side does not own —
            // and splitting a floating root now replaces it, which only its own stacking
            // order may do.
            self.split_in_branch(workspace, Layout::SplitV);
        }
    }

    pub fn consume_or_expel_window_left(&mut self, workspace: NodeKey, window: Option<&W::Id>) {
        self.consume_or_expel_window(workspace, window, Direction::Left);
    }

    pub fn consume_or_expel_window_right(&mut self, workspace: NodeKey, window: Option<&W::Id>) {
        self.consume_or_expel_window(workspace, window, Direction::Right);
    }

    pub fn toggle_full_width(&mut self, workspace: NodeKey) {
        let Some(tile) = self.focused_tile(workspace) else {
            return;
        };
        let id = tile.window().id().clone();
        let currently_fullscreen = self.window_owns_fullscreen(workspace, tile.window().id());
        let _ = self.set_fullscreen(workspace, &id, !currently_fullscreen);
    }

    fn toggle_window_dimension(
        &mut self,
        workspace: NodeKey,
        window: Option<&W::Id>,
        layout: Layout,
        presets: &[PresetSize],
        forwards: bool,
    ) {
        let Some(key) = self.window_target(workspace, window) else {
            return;
        };
        let Some((parent_key, child_idx, available, _, _)) =
            self.window_container_metrics(key, layout)
        else {
            return;
        };
        let current_percent = self.child_percent(parent_key, child_idx).unwrap_or(1.0);

        if let Some(percent) = self.cycle_presets(available, current_percent, presets, forwards) {
            if self.set_child_percent(parent_key, child_idx, layout, percent) {
                self.layout(workspace);
            }
        }
    }

    pub fn toggle_window_height(
        &mut self,
        workspace: NodeKey,
        window: Option<&W::Id>,
        forwards: bool,
    ) {
        let presets = self.options(workspace).layout.preset_window_heights.clone();
        self.toggle_window_dimension(workspace, window, Layout::SplitV, &presets, forwards);
    }

    pub fn toggle_window_width(
        &mut self,
        workspace: NodeKey,
        window: Option<&W::Id>,
        forwards: bool,
    ) {
        let presets = self.options(workspace).layout.preset_column_widths.clone();
        self.toggle_window_dimension(workspace, window, Layout::SplitH, &presets, forwards);
    }

    pub fn set_window_width(
        &mut self,
        workspace: NodeKey,
        window: Option<&W::Id>,
        change: SizeChange,
    ) {
        self.resize_window(
            workspace,
            window,
            ResizeRequest::Axis {
                axis: ResizeAxis::Horizontal,
                change,
            },
        );
    }

    pub fn set_window_height(
        &mut self,
        workspace: NodeKey,
        window: Option<&W::Id>,
        change: SizeChange,
    ) {
        self.resize_window(
            workspace,
            window,
            ResizeRequest::Axis {
                axis: ResizeAxis::Vertical,
                change,
            },
        );
    }

    /// Resolve a window-addressed resize and apply it to that container node.
    pub fn resize_window(
        &mut self,
        workspace: NodeKey,
        window: Option<&W::Id>,
        request: ResizeRequest,
    ) {
        let Some(target) = self.window_target(workspace, window) else {
            return;
        };
        self.resize_node(target, request);
    }

    /// Apply one semantic tiled resize to an explicit node in either layout branch.
    pub(super) fn resize_node(&mut self, target: NodeKey, request: ResizeRequest) {
        match request {
            ResizeRequest::Axis {
                axis: ResizeAxis::Horizontal,
                change,
            } => {
                let is_set = matches!(
                    change,
                    SizeChange::SetFixed(_) | SizeChange::SetProportion(_)
                );
                self.resize_node_with_reach(target, change, Layout::SplitH, ResizeReach::Siblings);
                if is_set {
                    // `resize set width` fills the omitted height with the target's current
                    // pending height and processes it second. The resulting zero delta still
                    // snaps fractions and arranges a matching vertical ancestor in sway.
                    self.resize_node_with_reach(
                        target,
                        SizeChange::AdjustFixed(0),
                        Layout::SplitV,
                        ResizeReach::Siblings,
                    );
                }
            }
            ResizeRequest::Axis {
                axis: ResizeAxis::Vertical,
                change,
            } => {
                let is_set = matches!(
                    change,
                    SizeChange::SetFixed(_) | SizeChange::SetProportion(_)
                );
                if is_set {
                    // sway parses `resize set` as a width-and-height command even when only
                    // height was written. Width is processed first with its current value,
                    // and a zero delta is observable because it arranges the resize parent.
                    self.resize_node_with_reach(
                        target,
                        SizeChange::AdjustFixed(0),
                        Layout::SplitH,
                        ResizeReach::Siblings,
                    );
                }
                self.resize_node_with_reach(target, change, Layout::SplitV, ResizeReach::Siblings);
            }
            ResizeRequest::Edge { direction, amount } => {
                let reach = if direction.is_leading() {
                    ResizeReach::Before
                } else {
                    ResizeReach::After
                };
                self.resize_node_with_reach(
                    target,
                    SizeChange::AdjustFixed(amount),
                    direction.split_layout(),
                    reach,
                );
            }
        }
    }

    /// Resize a window's share within its nearest ancestor split along `layout`'s axis.
    ///
    /// sway's `resize`, in two halves: the size asked for becomes an amount of the parent's
    /// extent, and the amount is taken from the siblings. Both halves are somewhere else —
    /// `percent_from_size_change` reads the request, `resize_child` moves the space — so this
    /// is only the sentence that joins them.
    fn resize_node_with_reach(
        &mut self,
        key: NodeKey,
        change: SizeChange,
        layout: Layout,
        reach: ResizeReach,
    ) {
        let Some(workspace) = self.workspace_of(key) else {
            return;
        };
        if !self.holds_node(key) || key == workspace {
            return;
        }
        let Some((parent_key, child_idx, available, child_count, _)) =
            self.resize_container_metrics(key, layout, reach)
        else {
            return;
        };
        if available <= 0.0 {
            return;
        }

        // sway works the amount out against the node the command was aimed at, and then hands
        // that amount to `container_resize_tiled`, which climbs to a *different* node — the
        // nearest ancestor with a sibling to take the space from — and moves it there. So
        // `resize set height 400` does not make anything 400 tall unless the two happen to be
        // the same node: it grows the ancestor by however much the target was short of 400.
        //
        //     container_resize_tiled(con, AXIS_VERTICAL, height->amount - con->pending.height);
        //
        // Reading the current size off the ancestor instead is the whole of the divergence in
        // `resize-a-branch-inside-a-stacked`: two levels of climb there, and tiri was setting
        // the workspace's split to the number the user asked of a window inside a stacked.
        // A ppt is read against the nearest ancestor parallel to the axis, which is a
        // different node from the one `find_resize_parent` climbed to, and then spent as a
        // delta from the target's own span. `resize set width 50 ppt` therefore means "half of
        // whatever holds me", not "half of whoever pays".
        let target_span = self.node_span(key, layout).unwrap_or(0.0);
        let reference = self.ppt_reference(key, layout);
        let pixels = match change {
            SizeChange::AdjustFixed(delta) => f64::from(delta),
            SizeChange::SetFixed(px) => f64::from(px) - target_span,
            SizeChange::AdjustProportion(ppt) => reference * ppt / 100.,
            SizeChange::SetProportion(ppt) => reference * ppt / 100. - target_span,
        };
        let delta = ResizeDelta { pixels };

        let min_size = match layout {
            Layout::SplitH => SWAY_MIN_TILED_WIDTH,
            Layout::SplitV => SWAY_MIN_TILED_HEIGHT,
            Layout::Tabbed | Layout::Stacked => return,
        };
        let child_spans = (0..child_count)
            .map(|idx| {
                self.child_rect_in(parent_key, idx)
                    .map(|rect| match layout {
                        Layout::SplitH => rect.size.w,
                        Layout::SplitV => rect.size.h,
                        Layout::Tabbed | Layout::Stacked => 0.0,
                    })
            })
            .collect::<Option<Vec<_>>>();
        let Some(child_spans) = child_spans else {
            return;
        };
        let space = ResizeSpace {
            min_size,
            child_spans,
        };
        if self.resize_child(parent_key, child_idx, layout, reach, delta, space) {
            if parent_key == workspace {
                self.layout(workspace);
            } else {
                self.layout_container_subtree(parent_key);
            }
        }
    }

    pub fn set_fullscreen(
        &mut self,
        workspace: NodeKey,
        window: &W::Id,
        is_fullscreen: bool,
    ) -> bool {
        let selected_container = self.selected_container_key();

        if is_fullscreen {
            if self.window_owns_fullscreen(workspace, window) {
                return false;
            }

            let Some(key) = self.tiled_window_key(workspace, window) else {
                return false;
            };

            let already_focused = self
                .focused_window()
                .is_some_and(|focused| focused.id() == window);
            if selected_container.is_none()
                && !already_focused
                && !self.focus_window_by_id(workspace, window)
            {
                return false;
            }

            let animate = !self.options(workspace).animations.off;
            if let Some(tile) = self.get_tile_mut(key) {
                tile.request_fullscreen(animate, None);
            }

            self.set_fullscreen_key(workspace, Some(key));
            self.layout(workspace);
            if let Some(selected_key) = selected_container {
                self.select_container(selected_key);
            }
            true
        } else {
            let Some(key) = self.tiled_window_key(workspace, window) else {
                return false;
            };
            let fullscreen_matches = self.window_owns_fullscreen(workspace, window);
            let working_area_size = self.working_area(workspace).size;
            let animate = !self.options(workspace).animations.off;
            let Some(tile) = self.get_tile_mut(key) else {
                return false;
            };
            let is_window_fullscreen = tile.window().pending_sizing_mode().is_fullscreen();
            if !is_window_fullscreen && !fullscreen_matches {
                return false;
            }

            // This is what actually takes the window out of fullscreen: the arrange pass sizes
            // a tile by asking whether its window is *still* pending fullscreen, so until a
            // non-fullscreen request has gone out there is nothing for it to notice. The size
            // is provisional — the arrange right below re-decides it against the slot the
            // window lands in.
            tile.request_tile_size(working_area_size, animate, None);

            if fullscreen_matches {
                self.set_fullscreen_key(workspace, None);
            }
            self.layout(workspace);
            if let Some(selected_key) = selected_container {
                self.select_container(selected_key);
            }
            true
        }
    }

    /// Toggle workspace fullscreen on an already resolved container target.
    ///
    /// Only a leaf asks its Wayland client to enter fullscreen. For a container, sway enlarges
    /// and arranges the complete subtree while every descendant remains an ordinary tiled
    /// client. The NodeKey carries that distinction across both tiled and floating branches.
    pub(super) fn toggle_fullscreen_container(&mut self, key: NodeKey) -> bool {
        let Some(workspace) = self.workspace_of(key) else {
            return false;
        };
        if self.container_info(key).is_none() {
            return false;
        }
        let is_fullscreen = self.fullscreen_key(workspace) == Some(key);

        if is_fullscreen {
            self.set_fullscreen_key(workspace, None);
        } else {
            self.set_fullscreen_key(workspace, Some(key));
        }

        self.layout(workspace);
        true
    }

    pub(super) fn sync_fullscreen_window(&mut self, workspace: NodeKey) {
        // A parked subtree carries client sizing state, but the destination workspace keeps
        // its own fullscreen pointer. Adopt the arriving fullscreen only when the destination
        // has none; otherwise revoke the stale request. This makes the single pointer true at
        // the protocol boundary too, rather than leaving two clients pending fullscreen.
        //
        // Both sides, because the workspace's fullscreen pointer is one for the whole
        // workspace and a floating window can hold it. A floating root is not in the tiled
        // root's child array, so sweeping that branch alone left two floating clients pending
        // fullscreen whenever a move brought one in beside another.
        let branches =
            std::iter::once(workspace).chain(self.floating_roots(workspace).collect::<Vec<_>>());
        let pending_keys: Vec<_> = branches
            .flat_map(|branch| self.leaf_keys_in_branch(branch))
            .filter(|key| {
                self.get_tile(*key)
                    .is_some_and(|tile| tile.window().pending_sizing_mode().is_fullscreen())
            })
            .collect();
        let authority = self.fullscreen_key(workspace);
        let keep = authority.or_else(|| pending_keys.first().copied());
        if authority.is_none() {
            if let Some(key) = keep {
                self.set_fullscreen_key(workspace, Some(key));
            }
        }

        let working_area_size = self.working_area(workspace).size;
        let animate = !self.options(workspace).animations.off;
        for key in pending_keys.into_iter().filter(|key| Some(*key) != keep) {
            let Some(tile) = self.get_tile_mut(key) else {
                continue;
            };
            tile.request_tile_size(working_area_size, animate, None);
        }
    }

    /// Inert: a tiled window has no position of its own to centre.
    ///
    /// It fills the slot its container hands it, so there is nothing here to move without
    /// inventing a meaning for centering inside a tree. sway says this by refusing `move
    /// position` on a tiled container; tiri says it by doing nothing, which
    /// `centering_a_tiled_window_does_nothing` pins.
    pub fn center_window(&mut self, _window: Option<&W::Id>) {}

    pub fn expand_column_to_available_width(&mut self, workspace: NodeKey) {
        let Some(idx) = self.focused_root_index(workspace) else {
            return;
        };
        let Some(root_key) = self.root_node_key(workspace) else {
            return;
        };
        if self.set_child_percent(root_key, idx, Layout::SplitH, 1.0) {
            self.layout(workspace);
        }
    }

    /// sway's `swap container with`: exchange the selected node and the window `target`.
    pub fn swap_selected_with_window(&mut self, target: &W::Id) -> bool {
        let Some(target) = self.window_key(target) else {
            return false;
        };
        self.swap_selected_nodes(target)
    }

    pub fn start_open_animation(&mut self, workspace: NodeKey, id: &W::Id) -> bool {
        let Some(key) = self.tiled_window_key(workspace, id) else {
            return false;
        };
        if let Some(tile) = self.get_tile_mut(key) {
            tile.start_open_animation();
            return true;
        }
        false
    }
    pub fn start_close_animation_for_window<R: NiriRenderer>(
        &mut self,
        workspace: NodeKey,
        closing: &mut Vec<ClosingWindow>,
        renderer: &mut R,
        window: &W::Id,
        blocker: crate::utils::transaction::TransactionBlocker,
    ) {
        let Some(key) = self.tiled_window_key(workspace, window) else {
            return;
        };

        let Some((rect, visible)) = self.leaf_layout(key).map(|info| (info.rect, info.visible))
        else {
            return;
        };

        let visible = visible
            && self
                .render_fullscreen_container(workspace)
                .is_none_or(|scope| self.is_descendant(key, scope));
        if !visible {
            return;
        }

        let Some(tile) = self.get_tile_mut(key) else {
            return;
        };

        let Some(snapshot) = tile.take_unmap_snapshot() else {
            return;
        };

        let tile_size = tile.tile_size();
        let tile_pos = rect.loc + tile.render_offset();

        let anim = Animation::new(
            self.clock.clone(),
            0.,
            1.,
            0.,
            self.options(workspace).animations.window_close.anim,
        );

        let scale = Scale::from(self.scale(workspace));
        let res = ClosingWindow::new(
            renderer.as_gles_renderer(),
            snapshot,
            scale,
            tile_size,
            tile_pos,
            blocker,
            anim,
        );
        match res {
            Ok(window) => {
                closing.push(window);
            }
            Err(err) => {
                warn!("error creating a closing window animation: {err:?}");
            }
        }
    }

    pub(super) fn refresh(
        &mut self,
        workspace: NodeKey,
        resize: &Option<InteractiveResizeState<W>>,
        is_active: bool,
        is_focused: bool,
    ) {
        let _span = tracy_client::span!("ContainerTree::refresh");
        self.apply_pending_layouts_and_deferred_relayout(workspace);
        let has_pending = self.has_pending_layouts(workspace);
        let mut layouts = std::mem::take(&mut self.render.state_layout_scratch);
        layouts.clear();
        {
            let _span = tracy_client::span!("ContainerTree::project_state_layouts_for_refresh");
            if has_pending {
                layouts.extend(
                    self.pending_leaf_layouts()
                        .map(|(branch, info)| LeafFrameInfo::new(branch, info)),
                );
            } else {
                let arena = &*self;
                layouts.extend(
                    arena
                        .leaf_layouts_in(workspace)
                        .map(|info| LeafFrameInfo::new(arena.branch_root(info.key), info)),
                );
            }
        }
        #[cfg(feature = "profile-with-tracy")]
        {
            tracy_client::plot!("layout.refresh_leaf_projections", layouts.len() as f64);
        }
        let tiled_root = workspace;
        layouts.retain(|info| info.branch == tiled_root);
        let focused_key = self.focused_key(workspace);
        let fullscreen_container = self.render_fullscreen_container(workspace);
        let fullscreen_id = self.pending_fullscreen_window(workspace).cloned();
        let windowed_fullscreen_id = if fullscreen_id.is_none() && fullscreen_container.is_none() {
            self.focused_tile(workspace).and_then(|tile| {
                tile.window()
                    .is_pending_windowed_fullscreen()
                    .then(|| tile.window().id().clone())
            })
        } else {
            None
        };

        let options = self.options(workspace).clone();
        let ctx = WindowStateContext {
            focused_key,
            workspace_active: is_active,
            deactivate_unfocused: options.deactivate_unfocused_windows && !is_focused,
            request_size: !has_pending,
            working_area_size: self.working_area(workspace).size,
            options: &options,
            fullscreen_id: fullscreen_id.as_ref(),
            windowed_fullscreen_id: windowed_fullscreen_id.as_ref(),
            has_fullscreen_container: fullscreen_container.is_some(),
            view_size: self.view_size(workspace),
        };
        // See the other state pass: never drive window state from a stale snapshot.
        let skip_state_pass = self.pending_layout_is_stale(workspace);
        let resize_data = self.interactive_resize_data_by_leaf(resize, &layouts);
        for info in &layouts {
            if skip_state_pass {
                break;
            }
            let in_fullscreen_container =
                fullscreen_container.is_some_and(|scope| self.is_descendant(info.key, scope));
            // Use O(1) key lookup instead of O(depth) path lookup.
            if let Some(tile) = self.get_tile_mut(info.key) {
                let resize = resize_data.get(&info.key).copied();
                Self::update_window_state(tile, info, resize, in_fullscreen_container, &ctx);
            }
        }
        self.render.state_layout_scratch = layouts;
    }
    pub fn render_above_top_layer(&self, workspace: NodeKey) -> bool {
        // Render above the top layer (e.g. waybar) for either fullscreen authority.
        self.render_fullscreen_container(workspace).is_some()
            || self.render_fullscreen_window(workspace).is_some()
            || self
                .focused_tile(workspace)
                .is_some_and(|tile| tile.window().is_pending_windowed_fullscreen())
    }

    pub fn popup_target_rect(
        &self,
        workspace: NodeKey,
        window: &W::Id,
    ) -> Option<Rectangle<f64, Logical>> {
        // Find the tile for this window and return its popup target rectangle
        for info in self.display_layouts(workspace) {
            if let Some(tile) = self.get_tile(info.key) {
                if tile.window().id() == window {
                    // Similar to tiling layout: constrain horizontally to window,
                    // vertically to the working area
                    let width = tile.window_size().w;
                    let height = self.working_area(workspace).size.h;

                    let mut target = Rectangle::from_size(Size::from((width, height)));
                    target.loc.y += self.working_area(workspace).loc.y;
                    target.loc.y -= info.rect.loc.y;
                    target.loc.y -= tile.window_loc().y;

                    return Some(target);
                }
            }
        }
        None
    }
}

impl<W: LayoutElement> ContainerArena<W> {
    pub(crate) fn layout_tree(&self, workspace: NodeKey) -> Option<LayoutTreeNode> {
        self.apply_workspace_fullscreen(workspace, self.build_workspace_layout_tree(workspace)?)
    }

    pub(crate) fn layout_tree_unfocused(&self, workspace: NodeKey) -> Option<LayoutTreeNode> {
        self.apply_workspace_fullscreen(
            workspace,
            self.build_workspace_layout_tree_unfocused(workspace)?,
        )
    }

    /// What a fullscreen node does to the tree everyone else reads.
    ///
    /// Two things sway's own tree says and the tiling tree does not, because both are the
    /// space's state rather than the tree's: a fullscreen leaf covers the output, so its
    /// rectangle is the output's and not the slot it came from; and a fullscreen node hides
    /// every leaf outside its subtree. The latter is the last clause of `view_is_visible`,
    /// which runs after the tab check so a hidden tab remains hidden inside a fullscreen
    /// container.
    pub(super) fn apply_workspace_fullscreen(
        &self,
        workspace: NodeKey,
        mut root: LayoutTreeNode,
    ) -> Option<LayoutTreeNode> {
        let Some(fullscreen_key) = self.fullscreen_key(workspace) else {
            return Some(root);
        };
        let fullscreen_windows: HashSet<u64> = self
            .tiles_in_branch(fullscreen_key)
            .into_iter()
            .map(|tile| tile.window().ipc_id())
            .collect();
        let fullscreen_leaf = self
            .get_tile(fullscreen_key)
            .map(|tile| tile.window().ipc_id());
        let fullscreen_leaf_uses_output = self
            .leaf_layout(fullscreen_key)
            .is_some_and(|info| info.workspace_fullscreen);
        let view = LayoutTreeRect {
            x: 0.0,
            y: 0.0,
            width: self.view_size(workspace).w,
            height: self.view_size(workspace).h,
        };

        fn walk(
            node: &mut LayoutTreeNode,
            fullscreen_windows: &HashSet<u64>,
            fullscreen_leaf: Option<u64>,
            fullscreen_leaf_uses_output: bool,
            view: LayoutTreeRect,
        ) {
            if let Some(window) = node.window_id {
                if !fullscreen_windows.contains(&window) {
                    node.visible = false;
                } else if Some(window) == fullscreen_leaf && fullscreen_leaf_uses_output {
                    // An ordinary `arrange_workspace` gives a fullscreen view the output.
                    // A direct `arrange_container(parent)` is different: the leaf keeps the
                    // ordinary (possibly zero) pending box from that pass. The cached layout
                    // records which route produced it, so ownership and geometry remain
                    // separate meanings.
                    node.rect = Some(view);
                }
                return;
            }
            for child in &mut node.children {
                walk(
                    child,
                    fullscreen_windows,
                    fullscreen_leaf,
                    fullscreen_leaf_uses_output,
                    view,
                );
            }
            node.visible = node.children.iter().any(|child| child.visible);
        }

        walk(
            &mut root,
            &fullscreen_windows,
            fullscreen_leaf,
            fullscreen_leaf_uses_output,
            view,
        );
        Some(root)
    }
}

impl<W: LayoutElement> ContainerArena<W> {
    fn update_window_state(
        tile: &mut Tile<W>,
        info: &LeafFrameInfo,
        interactive_resize: Option<InteractiveResizeData>,
        in_fullscreen_container: bool,
        ctx: &WindowStateContext<'_, W>,
    ) {
        let &WindowStateContext {
            focused_key,
            workspace_active,
            deactivate_unfocused,
            request_size,
            working_area_size,
            options,
            fullscreen_id,
            windowed_fullscreen_id,
            has_fullscreen_container,
            view_size,
        } = ctx;

        let window_id = tile.window().id().clone();
        let is_focused_tile = focused_key == Some(info.key);
        let is_fullscreen_tile = fullscreen_id.is_some_and(|id| id == &window_id);
        let is_windowed_fullscreen_tile = windowed_fullscreen_id.is_some_and(|id| id == &window_id);
        let is_fullscreen_like_tile = is_fullscreen_tile || is_windowed_fullscreen_tile;
        let has_fullscreen_like =
            fullscreen_id.is_some() || windowed_fullscreen_id.is_some() || has_fullscreen_container;
        let is_in_fullscreen_scope = is_fullscreen_like_tile || in_fullscreen_container;

        if request_size {
            if is_fullscreen_tile {
                tile.request_fullscreen(false, None);
            } else if is_windowed_fullscreen_tile {
                tile.request_windowed_fullscreen(false, None);
            } else {
                let target_size = Size::from((info.rect.size.w, info.rect.size.h));
                tile.request_tile_size(target_size, false, None);
            }
        }

        let mut active = workspace_active && is_focused_tile;

        if has_fullscreen_like && !is_in_fullscreen_scope {
            active = false;
        } else if deactivate_unfocused {
            active &= info.visible;
        }

        let active_in_column = is_focused_tile && (!has_fullscreen_like || is_in_fullscreen_scope);

        let window = tile.window_mut();
        window.set_active_in_column(active_in_column);
        window.set_floating(false);
        window.set_activated(active);
        window.set_interactive_resize(interactive_resize);

        let border_config = options.layout.border.merged_with(&window.rules().border);

        let bounds = if is_fullscreen_like_tile {
            view_size.to_i32_floor()
        } else {
            let max_bounds = compute_toplevel_bounds(
                border_config,
                working_area_size,
                Size::from((0.0, 0.0)),
                options.layout.gaps,
            );
            let mut logical_bounds: Size<i32, Logical> =
                Size::from((info.rect.size.w, info.rect.size.h)).to_i32_floor();
            logical_bounds.w = logical_bounds.w.min(max_bounds.w);
            logical_bounds.h = logical_bounds.h.min(max_bounds.h);
            logical_bounds
        };

        window.set_bounds(bounds);

        match window.configure_intent() {
            ConfigureIntent::CanSend | ConfigureIntent::ShouldSend => {
                window.send_pending_configure();
            }
            _ => {}
        }

        window.refresh();
    }
}

impl RootTilingSubtree {
    /// A tiled container moved to another workspace gets a new share there while every sizing
    /// value below it remains attached to its descendant node.
    ///
    /// sway/commands/move.c:198-239
    pub(super) fn prepare_for_workspace_move<W: LayoutElement>(
        &mut self,
        containers: &mut ContainerTree<W>,
    ) {
        containers.unset_node_fractions(self.subtree);
    }

    pub fn tiles<'a, W: LayoutElement>(
        &self,
        containers: &'a ContainerTree<W>,
    ) -> Vec<&'a Tile<W>> {
        containers.tiles_in_branch(self.subtree)
    }

    pub fn from_subtree(subtree: NodeKey) -> Self {
        Self { subtree }
    }

    pub fn into_subtree(self) -> NodeKey {
        self.subtree
    }
}

fn compute_toplevel_bounds(
    border_config: Border,
    working_area_size: Size<f64, Logical>,
    extra_size: Size<f64, Logical>,
    gaps: f64,
) -> Size<i32, Logical> {
    let mut border = 0.0;
    if !border_config.off {
        border = border_config.width * 2.0;
    }

    Size::from((
        f64::max(
            working_area_size.w - gaps * 2.0 - extra_size.w - border,
            1.0,
        ),
        f64::max(
            working_area_size.h - gaps * 2.0 - extra_size.h - border,
            1.0,
        ),
    ))
    .to_i32_floor()
}

fn edge_visibility_for_tile(
    options: &Options,
    layout_rect: Rectangle<f64, Logical>,
    tile_rect: Rectangle<f64, Logical>,
    scale: f64,
    is_single_window: bool,
) -> FocusRingEdges {
    if options.layout.hide_edge_borders_smart && is_single_window {
        return FocusRingEdges::none();
    }

    let mut edges = FocusRingEdges::all();
    let hide_mode = options.layout.hide_edge_borders;
    if hide_mode == HideEdgeBorders::None {
        return edges;
    }

    let eps = 0.5 / scale.max(1e-6);
    let left = (tile_rect.loc.x - layout_rect.loc.x).abs() <= eps;
    let right = (tile_rect.loc.x + tile_rect.size.w - (layout_rect.loc.x + layout_rect.size.w))
        .abs()
        <= eps;
    let top = (tile_rect.loc.y - layout_rect.loc.y).abs() <= eps;
    let bottom = (tile_rect.loc.y + tile_rect.size.h - (layout_rect.loc.y + layout_rect.size.h))
        .abs()
        <= eps;

    let hide_horizontal = matches!(
        hide_mode,
        HideEdgeBorders::Horizontal | HideEdgeBorders::Both
    );
    let hide_vertical = matches!(hide_mode, HideEdgeBorders::Vertical | HideEdgeBorders::Both);

    if hide_horizontal {
        if top {
            edges.top = false;
        }
        if bottom {
            edges.bottom = false;
        }
    }

    if hide_vertical {
        if left {
            edges.left = false;
        }
        if right {
            edges.right = false;
        }
    }

    edges
}

fn split_indicator_edge_for_tile<W: LayoutElement>(
    tree: &ContainerArena<W>,
    key: NodeKey,
    edges: FocusRingEdges,
) -> Option<FocusRingIndicatorEdge> {
    let layout = tree.single_child_split_layout(key)?;
    match layout {
        Layout::SplitH => edges.right.then_some(FocusRingIndicatorEdge::Right),
        Layout::SplitV => edges.bottom.then_some(FocusRingIndicatorEdge::Bottom),
        _ => None,
    }
}

/// The leaf layouts of one branch — the tiled side, or one floating group.
///
/// One tree holds both sides now, so a consumer that means "my leaves" has to say which
/// branch. Identity and order come from the live tree; the cache contributes presentation only.
pub(super) fn branch_display_layouts<'a, W: LayoutElement>(
    tree: &'a ContainerArena<W>,
    branch: NodeKey,
) -> impl DoubleEndedIterator<Item = &'a LeafLayoutInfo> + 'a {
    // The committed layouts are what is on screen, so they are the answer whenever this
    // branch has any. It has none only before its first arrange has landed, and then the
    // pending ones are the only answer there is — asking a neighbour's would be worse than
    // saying nothing.
    let keys = tree.leaf_keys_in_branch(branch);
    let committed = tree.leaf_layout_cache();
    let source = if keys.iter().any(|key| committed.contains_key(key)) {
        committed
    } else {
        tree.pending_leaf_layouts_for(branch).unwrap_or(committed)
    };
    keys.into_iter().filter_map(move |key| source.get(&key))
}

/// Span available to a container's children after subtracting inter-child gaps.
pub(super) fn available_span(gap: f64, total: f64, child_count: usize) -> f64 {
    if child_count == 0 {
        return 0.0;
    }
    (total - gap * (child_count as f64 - 1.0)).max(0.0)
}

/// The share of `available` a size request asks for.
///
/// One rule for both sides of the workspace, because sway has one. `container_is_floating`
/// (sway/tree/container.c:1104-1113) is true only for a floating *root* — a node with no
/// parent sitting in `ws->floating` — so every child inside a floating group goes through
/// `resize_set_tiled`/`resize_adjust_tiled` exactly like a tiled one. Only the group itself
/// takes the floating path, and that one is measured in pixels against
/// `floating_calculate_constraints`, not in shares.
///
/// A proportion is always hundredths. The unit is lexical in sway (`MOVEMENT_UNIT_PPT`, from
/// the `ppt` suffix) and lexical here too — `SizeChange` is built by the `%` suffix in
/// `tiri-ipc/src/lib.rs:2136`. Deducing it from the magnitude instead, which both copies of
/// this used to do and disagreed about, made `0.5%` mean either 0.5% or 50%.
///
/// `ppt_reference` is the extent hundredths are read against, which is not `available`: see
/// [`ContainerTree::ppt_reference`].
///
/// There is no floor here. sway's minimum is `MIN_SANE_W`/`MIN_SANE_H` (sway/tree/node.h:8-9)
/// in pixels, checked while the space is moved and aborting the whole resize rather than
/// clamping it — so a floor in share space would be a second, invented minimum.
pub(super) fn percent_from_size_change(
    current_percent: f64,
    available: f64,
    ppt_reference: impl FnOnce() -> f64,
    change: SizeChange,
) -> f64 {
    if available <= 0.0 {
        return current_percent;
    }

    // Taken lazily: the pixel forms never read it, and working it out means climbing the
    // tree, which interactive resize would otherwise pay for on every pointer motion.
    let hundredths = |ppt: f64| ppt_reference() * ppt / 100. / available;
    let percent = match change {
        SizeChange::SetFixed(px) => f64::from(px) / available,
        SizeChange::AdjustFixed(delta) => current_percent + f64::from(delta) / available,
        SizeChange::SetProportion(ppt) => hundredths(ppt),
        SizeChange::AdjustProportion(ppt) => current_percent + hundredths(ppt),
    };

    percent.clamp(0.0, 1.0)
}

/// What a tile needs to know about the space it is in: see [`ContainerTree::tile_config`].
#[derive(Debug, Clone)]
pub(super) struct TileConfig {
    pub(super) view_size: Size<f64, Logical>,
    pub(super) scale: f64,
    pub(super) options: Rc<Options>,
}

/// A tree mutation's report of whether it changed anything.
///
/// Implemented for `bool` (the op tells us) and for `()` (the op has no failure signal, so
/// assume it changed). Used by the spaces' `mutate_tree` so a mutation can never be
/// committed without the matching relayout.
pub(super) trait TreeMutation {
    fn changed(&self) -> bool;
}

impl TreeMutation for bool {
    fn changed(&self) -> bool {
        *self
    }
}

impl TreeMutation for () {
    fn changed(&self) -> bool {
        true
    }
}

/// A mutation that yields something only when it did something, such as taking a subtree
/// out of the tree.
impl<T> TreeMutation for Option<T> {
    fn changed(&self) -> bool {
        self.is_some()
    }
}
