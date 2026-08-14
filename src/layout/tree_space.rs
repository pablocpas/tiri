//! The workspace's layout space: the arena its windows live in, and the tiled side.
//!
//! sway's workspace holds two lists — `ws->tiling` and `ws->floating` — over one set of
//! containers, and the same container moves between them without being rebuilt. This is that
//! set. Windows sit in a tree whose internal nodes are containers with a layout mode (SplitH,
//! SplitV, Tabbed, Stacked) and whose leaves are tiles; the tiled side hangs off the
//! workspace root and each floating group is a root of its own.
//!
//! Node access is O(1), and node keys remain stable when a subtree changes workspace or
//! output.
//!
//! What lives here beyond the arena is what the workspace answers for as a whole: its box,
//! its scale, its options, its fullscreen, its closing animations. The floating side's own
//! state — where each group sits, what order they stack in — is in [`super::floating`].

use std::cell::{RefCell, RefMut};
use std::collections::{HashMap, HashSet};
use std::rc::Rc;
use std::time::Duration;

use smithay::backend::renderer::element::Kind;
use smithay::backend::renderer::gles::GlesRenderer;
use smithay::utils::{Logical, Point, Rectangle, Scale, Size};
use tiri_config::utils::MergeWith as _;
use tiri_config::{Border, HideEdgeBorders, PresetSize, TabBar};
use tiri_ipc::{ColumnDisplay, LayoutTreeNode, LayoutTreeRect, SizeChange};

use super::closing_window::{ClosingWindow, ClosingWindowRenderElement};
use super::container::{
    ContainerMetrics, ContainerTree, DetachedContainer, DetachedNode, Direction, Layout,
    LeafLayoutInfo, NodeKey, ResizeDelta, ResizeReach, ResizeSpace, ResizeTarget,
};
use super::focus_ring::{
    render_container_selection, ContainerSelectionStyle, FocusRingEdges, FocusRingIndicatorEdge,
    FocusRingRenderElement,
};
use super::legacy_column::{Column, ColumnWidth};
use super::monitor::{InsertPosition, SplitIndicator};
use super::tile::{Tile, TileRenderElement};
use super::viewport::FixedViewport;
use super::{
    ConfigureIntent, InteractiveResizeData, LayoutCycleEntry, LayoutElement, Options, RemovedTile,
    ResizeAxis, ResizeHit, ResizeRequest,
};
use crate::animation::{Animation, Clock};
use crate::layout::tab_bar::{
    render_tab_bar, tab_bar_hit_index, tab_bar_state_from_info, TabBarCacheEntry,
    TabBarRenderOutput,
};
use crate::niri_render_elements;
use crate::render_helpers::offscreen::{OffscreenBuffer, OffscreenRenderElement};
use crate::render_helpers::primary_gpu_texture::PrimaryGpuTextureRenderElement;
use crate::render_helpers::renderer::NiriRenderer;
use crate::render_helpers::solid_color::{SolidColorBuffer, SolidColorRenderElement};
use crate::render_helpers::texture::TextureRenderElement;
use crate::render_helpers::xray::XrayPos;
use crate::render_helpers::{RenderCtx, RenderTarget};
use crate::utils::round_logical_in_physical_max1;
use crate::utils::transaction::Transaction;
use crate::utils::ResizeEdge;
use crate::window::ResolvedWindowRules;
use log::warn;

// sway 1.12's MIN_SANE_W and MIN_SANE_H. Tiled resize refuses the entire operation when
// any participating sibling would cross the relevant pixel floor.
const SWAY_MIN_TILED_WIDTH: f64 = 100.0;
const SWAY_MIN_TILED_HEIGHT: f64 = 60.0;

// ============================================================================
// MAIN STRUCTURES - i3-style container tree implementation
// ============================================================================

/// i3-style tiling space using hierarchical containers
#[derive(Debug)]
pub struct TreeSpace<W: LayoutElement> {
    /// Container tree managing window layout
    tree: ContainerTree<W>,
    /// Workspace-level layout state (sway workspace->layout equivalent).
    /// Previous workspace split layout (sway workspace->prev_split_layout equivalent).
    /// View size (output size)
    view_size: Size<f64, Logical>,
    /// Working area (view_size minus gaps/bars)
    working_area: Rectangle<f64, Logical>,
    /// Viewport behavior. Fixed for i3/sway tiling; kept as a component to isolate niri merge
    /// points that still talk about viewport gestures.
    viewport: FixedViewport,
    /// Display scale
    scale: f64,
    /// Animation clock
    clock: Clock,
    /// Ongoing interactive resize.
    interactive_resize: Option<InteractiveResizeState<W>>,
    /// Layout options
    options: Rc<Options>,
    /// Tab bars already drawn, for both sides of the workspace.
    ///
    /// One cache, because a node key is unique across the arena; two would have to agree on
    /// which side a container is on, and a container changes sides. Entries are dropped when
    /// their container leaves the tree, rather than when a frame does not redraw them: a
    /// fullscreen draws no bars at all and is not a reason to forget every one of them.
    tab_bar_cache: RefCell<HashMap<NodeKey, TabBarCacheEntry>>,
    /// Whether this workspace is the active one.
    is_active: bool,
    /// Which of the workspace's two sides holds the focus.
    ///
    /// "Active" is not one value for the workspace: a tab bar is drawn focused when the side
    /// it is on is the side with focus, and only one side can be. Keeping the workspace's own
    /// activeness beside which side has it lets both sides ask the same question and get
    /// their own answer — which is what two `is_active` fields were for before they were
    /// merged into one that could only be right for whichever side wrote it last.
    floating_has_focus: bool,
    /// Windows in the closing animation.
    closing_windows: Vec<ClosingWindow>,
    /// Cached offscreen texture for overview rendering.
    overview_offscreen: OffscreenBuffer,
    /// Stable workspace-sized background used under the overview offscreen.
    overview_background: SolidColorBuffer,
}

/// A leaf identified for hit-testing: stable node identity and on-screen rect.
type LeafHit = (NodeKey, Rectangle<f64, Logical>);

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

#[derive(Debug, Clone)]
struct InteractiveResizeState<W: LayoutElement> {
    window: W::Id,
    data: InteractiveResizeData,
    horizontal: Option<ResizeTarget>,
    vertical: Option<ResizeTarget>,
}

niri_render_elements! {
    TreeSpaceRenderElement<R> => {
        Tile = TileRenderElement<R>,
        TabBar = PrimaryGpuTextureRenderElement,
        ClosingWindow = ClosingWindowRenderElement,
        ContainerSelection = FocusRingRenderElement,
        SolidColor = SolidColorRenderElement,
        Offscreen = OffscreenRenderElement,
    }
}

/// Detached top-level tiling subtree.
///
/// This is the internal unit moved across workspaces and monitors in the i3-style tree.
#[derive(Debug)]
pub struct RootTilingSubtree<W: LayoutElement> {
    /// Detached subtree that preserves container structure.
    subtree: DetachedNode<W>,
}

/// Window height specification for tiling layout
#[derive(Debug, Default, Clone, Copy, PartialEq)]
pub enum WindowHeight {
    #[default]
    Auto,
    Fixed(i32),
}

// ============================================================================
// TreeSpace Implementation
// ============================================================================

impl<W: LayoutElement> TreeSpace<W> {
    fn tiled_window_key(&self, id: &W::Id) -> Option<NodeKey> {
        let key = self.tree.window_key(id)?;
        (self.tree.branch_root(key) == self.tree.workspace_root()).then_some(key)
    }

    /// Match sway's `view_map` arrange boundary after inserting a new tiled window.
    ///
    /// A view inserted below a container arranges that container against its current pending
    /// box. A view inserted directly below the workspace node arranges the
    /// whole workspace instead. Keeping this decision here also avoids reconstructing a path:
    /// the inserted leaf and its parent are already stable `NodeKey`s.
    fn layout_after_tiled_insert(&mut self, id: &W::Id) {
        let root = self.tree.workspace_root();
        let parent = self
            .tiled_window_key(id)
            .and_then(|key| self.tree.parent_of(key));

        if let Some(parent) = parent.filter(|parent| *parent != root) {
            self.tree.layout_container_subtree(parent);
        } else {
            self.tree.layout();
        }
    }

    fn render_fullscreen_window(&self) -> Option<W::Id> {
        let id = self.pending_fullscreen_window()?;
        let key = self.tiled_window_key(id)?;
        let tile = self.tree.get_tile(key)?;
        tile.window()
            .sizing_mode()
            .is_fullscreen()
            .then(|| id.clone())
    }

    fn tiled_fullscreen_key(&self) -> Option<NodeKey> {
        let key = self.tree.fullscreen_key()?;
        (self.tree.holds_node(key) && self.tree.branch_root(key) == self.tree.workspace_root())
            .then_some(key)
    }

    /// A tiled container that is currently the workspace fullscreen authority.
    ///
    /// Unlike a fullscreen leaf, this has no client commit to wait for: its descendants stay
    /// ordinary tiled clients and the compositor makes their branch exclusive itself.
    fn render_fullscreen_container(&self) -> Option<NodeKey> {
        let key = self.tiled_fullscreen_key()?;
        self.tree.container_info(key)?;
        Some(key)
    }

    fn fullscreen_render_state(&self) -> FullscreenRenderState<W::Id> {
        let container = self.render_fullscreen_container();
        let fullscreen_id = self.render_fullscreen_window();
        let windowed_fullscreen_id = if fullscreen_id.is_none() && container.is_none() {
            self.focused_tile().and_then(|tile| {
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

    fn pending_fullscreen_window(&self) -> Option<&W::Id> {
        self.tiled_fullscreen_key()?;
        self.tree.fullscreen_leaf_window_id()
    }

    /// The workspace's arena, holding both of its sides.
    ///
    /// sway keeps one workspace with two lists over one set of containers. This type is that
    /// workspace's set, which is why the floating side asks it for the arena instead of
    /// keeping one: not a consumer lending out its own, a workspace answering for itself.
    pub(super) fn tree(&self) -> &ContainerTree<W> {
        &self.tree
    }

    pub(super) fn tree_mut(&mut self) -> &mut ContainerTree<W> {
        &mut self.tree
    }

    /// The tiled side's cached leaf layouts. The floating groups are branches of the same
    /// tree and are laid out by the same pass, so "the layouts" has to say which branch.
    fn display_layouts(&self) -> impl DoubleEndedIterator<Item = &LeafLayoutInfo> + '_ {
        branch_display_layouts(&self.tree, self.tree.workspace_root())
    }

    fn focused_key(&self) -> Option<NodeKey> {
        self.tree
            .focus_inactive_view_in_branch(self.tree.workspace_root())
    }

    fn focused_tile(&self) -> Option<&Tile<W>> {
        self.focused_key().and_then(|key| self.tree.get_tile(key))
    }

    fn effective_tab_bar_config(&self) -> TabBar {
        self.options.layout.tab_bar.clone()
    }

    /// Whether the workspace itself is what the tree currently has selected.
    ///
    /// An empty workspace has nothing else a command could be aimed at; otherwise this is
    /// what `focus parent` on a top-level node leaves behind. A focused *leaf* never selects
    /// the workspace, even when its parent is the workspace — measured against sway 1.11,
    /// which builds a container for such a command instead of retargeting the workspace.
    pub fn workspace_is_selected(&self) -> bool {
        self.tree.workspace_is_selected()
    }

    fn available_span(&self, total: f64, child_count: usize) -> f64 {
        available_span(self.options.layout.gaps, total, child_count)
    }

    /// The gap between a branch's children.
    ///
    /// `gaps` is a workspace setting, and a floating group is not laid out in the workspace's
    /// box: it is laid out in its own, which is exactly the size the user dragged it to. A gap
    /// inside that box would come out of the window rather than out of the workspace.
    pub(super) fn branch_gap(&self, branch: NodeKey) -> f64 {
        if branch == self.tiled_branch() {
            self.options.layout.gaps
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
        self.tree
            .find_parent_with_layout(key, layout)
            .and_then(|(parent, _)| self.tree.node_span(parent, layout))
            .unwrap_or_else(|| match layout {
                Layout::SplitH => self.working_area().size.w,
                Layout::SplitV => self.working_area().size.h,
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
    fn window_target(&self, window: Option<&W::Id>) -> Option<NodeKey> {
        match window {
            Some(id) => self.tiled_window_key(id),
            None => self.tree.branch_position(self.tree.workspace_root()),
        }
    }

    fn window_container_metrics(&self, key: NodeKey, layout: Layout) -> Option<ContainerMetrics> {
        let (parent_key, child_idx) = self.tree.find_parent_with_layout(key, layout)?;
        self.container_metrics(parent_key, child_idx, layout)
    }

    fn resize_container_metrics(
        &self,
        key: NodeKey,
        layout: Layout,
        reach: ResizeReach,
    ) -> Option<ContainerMetrics> {
        let (parent_key, child_idx) = self.tree.find_resize_parent(key, layout, reach)?;
        let (container_layout, rect, child_count) = self.tree.container_info(parent_key)?;
        if container_layout != layout || child_count == 0 {
            return None;
        }
        let available = self
            .tree
            .child_resize_total(parent_key, child_idx, layout)?;
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
        let (container_layout, rect, child_count) = self.tree.container_info(parent_key)?;
        if container_layout != layout || child_count == 0 {
            return None;
        }

        let branch = self.tree.branch_root(parent_key);
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
        let (parent_key, child_idx) = self.tree.find_parent_with_layout(key, layout)?;
        self.container_metrics(parent_key, child_idx, layout)
    }

    fn selected_geometry(&self) -> Option<Rectangle<f64, Logical>> {
        let key = self.tree.selected_layout_parent_key()?;

        // For container selection visuals, prefer the on-screen leaf geometry under this
        // container. This stays in sync with what is currently rendered even when the
        // container's cached geometry is in transition.
        let mut bounds: Option<Rectangle<f64, Logical>> = None;
        for info in self
            .display_layouts()
            .filter(|info| self.tree.is_descendant(info.key, key))
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

        bounds.or_else(|| self.tree.container_info(key).map(|(_, rect, _)| rect))
    }

    pub fn selected_is_container(&self) -> bool {
        self.selected_container_in(self.tree.workspace_root())
    }

    /// Whether a child-laying node is selected in the tiled branch. Unlike the command-facing
    /// container query, this includes the workspace node so `focus parent` always has a visual
    /// result.
    fn selected_layout_parent_in_tiling(&self) -> bool {
        self.tree
            .selected_layout_parent_key()
            .is_some_and(|key| self.tree.branch_root(key) == self.tree.workspace_root())
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
        self.tree
            .selected_container_key()
            .is_some_and(|key| self.tree.branch_root(key) == branch)
    }

    /// The layout a command in this branch would be read against: the selected container's
    /// when there is one, otherwise the layout of whatever holds the branch's focus.
    pub(super) fn selection_layout_in(&self, branch: NodeKey) -> Option<Layout> {
        let key = self.tree.branch_position(branch)?;
        if self.selected_container_in(branch) {
            if let Some(info) = self.tree.container_info(key) {
                return Some(info.0);
            }
        }
        self.tree.layout_owning(key)
    }

    /// Whether this branch's root is a container tiri added rather than one the model has.
    ///
    /// A workspace is a container sway really has, so a command aimed at it has something to
    /// act on however few children it holds. A floating group's root is not: sway's
    /// `ws->floating` holds whatever was floated, a view included, while tiri always wraps,
    /// and a layout command aimed at that wrapper would be acting on a container sway would
    /// say does not exist. This is the open entry in the parity ledger, seen from the side
    /// that has to work around it.
    pub(super) fn branch_root_is_implicit(&self, branch: NodeKey) -> bool {
        if branch == self.tree.workspace_root() || self.selected_container_in(branch) {
            return false;
        }

        let Some(key) = self.tree.branch_position(branch) else {
            return false;
        };
        if self.tree.branch_relative_path(key).as_deref() != Some(&[0]) {
            return false;
        }
        let Some(root) = self.tree.branch_container(branch) else {
            return false;
        };
        root.child_count() == 1
            && !root.is_user_container()
            && matches!(root.layout(), Layout::SplitH | Layout::SplitV)
    }

    /// A layout command needs a container to act on. Vacuous on the tiled side, where the
    /// branch root is the workspace.
    fn branch_has_layout_target(&self, branch: NodeKey) -> bool {
        self.selection_layout_in(branch).is_some() && !self.branch_root_is_implicit(branch)
    }

    pub(super) fn split_in_branch(&mut self, branch: NodeKey, layout: Layout) -> bool {
        let target = self.tree.command_target_in(branch);
        let changed = self.tree.split_target(layout, target);
        if changed {
            self.tree.layout();
        }
        changed
    }

    pub(super) fn unsplit_in_branch(&mut self, branch: NodeKey) -> Option<NodeKey> {
        let target = self.tree.command_target_in(branch);
        let root = self.tree.unsplit_target(target)?;
        self.tree.layout();
        Some(root)
    }

    pub(super) fn set_layout_in_branch(&mut self, branch: NodeKey, layout: Layout) -> bool {
        if !self.branch_has_layout_target(branch) {
            return false;
        }
        let target = self.tree.command_target_in(branch);
        let changed = self.tree.set_layout_for_target(layout, target);
        if changed {
            self.tree.layout();
        }
        changed
    }

    pub(super) fn toggle_split_in_branch(&mut self, branch: NodeKey) -> bool {
        if !self.branch_has_layout_target(branch) {
            return false;
        }
        let target = self.tree.command_target_in(branch);
        let changed = self.tree.toggle_split_for_target(target);
        if changed {
            self.tree.layout();
        }
        changed
    }

    pub(super) fn toggle_layout_all_in_branch(&mut self, branch: NodeKey) -> bool {
        if !self.branch_has_layout_target(branch) {
            return false;
        }
        let target = self.tree.command_target_in(branch);
        let changed = self.tree.toggle_layout_all_for_target(target);
        if changed {
            self.tree.layout();
        }
        changed
    }

    pub(super) fn set_default_layout_in_branch(&mut self, branch: NodeKey) -> bool {
        if !self.branch_has_layout_target(branch) {
            return false;
        }
        let target = self.tree.command_target_in(branch);
        let changed = self.tree.set_default_layout_for_target(target);
        if changed {
            self.tree.layout();
        }
        changed
    }

    pub(super) fn toggle_layout_cycle_in_branch(
        &mut self,
        branch: NodeKey,
        cycle: &[LayoutCycleEntry],
    ) -> bool {
        if !self.branch_has_layout_target(branch) {
            return false;
        }
        let target = self.tree.command_target_in(branch);
        let changed = self.tree.toggle_layout_cycle_for_target(target, cycle);
        if changed {
            self.tree.layout();
        }
        changed
    }

    /// What `close` would close, aimed at this branch.
    pub(super) fn close_window_ids_in_branch(&self, branch: NodeKey) -> Vec<W::Id> {
        if self.selected_container_in(branch) {
            if let Some(key) = self.tree.selected_container_key() {
                return self.tree.window_ids_under(key);
            }
        }
        self.tree
            .focused_window_in_branch(branch)
            .map(|window| vec![window.id().clone()])
            .unwrap_or_default()
    }

    pub(super) fn close_window_ids_for_active_selection(&self) -> Vec<W::Id> {
        if self.selected_is_container() {
            if let Some(key) = self.tree.selected_container_key() {
                return self.tree.window_ids_under(key);
            }
        }

        self.active_window()
            .map(|window| vec![window.id().clone()])
            .unwrap_or_default()
    }

    pub(super) fn take_selected_subtree(&self) -> Option<(NodeKey, Rectangle<f64, Logical>)> {
        let key = self.tree.selected_node_key()?;
        self.tree.container_info(key)?;
        let rect = self.default_floating_container_rect();
        Some((key, rect))
    }

    /// sway's initial geometry for a container without a view when it starts floating.
    fn default_floating_container_rect(&self) -> Rectangle<f64, Logical> {
        let area = self.working_area;
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
    ) -> Option<(NodeKey, Rectangle<f64, Logical>)> {
        // What sway floats here is a container, and a container's default floating size is
        // `container_floating_set_default_size`: half the workspace by three quarters of it,
        // centred. `floating_natural_resize` overwrites that with the client's own size only
        // when there is a view to ask, and a wrapper around the workspace's children is not
        // one — which is why the number a floating *window* no longer gets is still this
        // one's.
        let rect = self.default_floating_container_rect();
        (!self.tree.is_empty()).then_some((self.tree.workspace_root(), rect))
    }

    pub(super) fn subtree_for_window_floating(
        &self,
        id: &W::Id,
    ) -> Option<(NodeKey, Rectangle<f64, Logical>)> {
        let key = self.tiled_window_key(id)?;
        let rect = self
            .tree
            .fullscreen_restore_geometry(key)
            .filter(|rect| rect.size.w > 0.0 && rect.size.h > 0.0)
            .or_else(|| {
                self.display_layouts()
                    .find(|info| info.key == key)
                    .map(|info| info.rect)
            })?;
        Some((key, rect))
    }

    /// Run a mutation on the tree and relayout when it reports a change.
    ///
    /// Every structural mutation must be followed by a relayout; routing them through this
    /// combinator makes that impossible to forget.
    fn mutate_tree<R: TreeMutation>(&mut self, f: impl FnOnce(&mut ContainerTree<W>) -> R) -> R {
        let result = f(&mut self.tree);
        if result.changed() {
            self.tree.layout();
        }
        // Whatever the mutation reported, the addresses beside the cached geometry describe
        // the tree and the tree may have moved. Keeping a derived field derived is cheaper
        // than trusting every path through the tree to say so.
        self.tree.readdress_leaf_layouts();
        result
    }

    /// Per-leaf resize edges for an in-flight interactive resize, keyed by node.
    ///
    /// Computed up front so the caller can hold mutable tile borrows while iterating.
    fn interactive_resize_data_by_leaf(
        &self,
        layouts: &[LeafLayoutInfo],
    ) -> HashMap<NodeKey, InteractiveResizeData> {
        let Some(resize) = self.interactive_resize.as_ref() else {
            return HashMap::new();
        };

        layouts
            .iter()
            .filter_map(|info| {
                let edges = self.tree.resize_edges_for_leaf(
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
        let Some(available) = self.tree.resize_available_span(target, layout) else {
            return false;
        };

        let delta = if inverted { -delta } else { delta };
        let new_span = (target.original_span.max(1.0) + delta).round() as i32;
        let current_percent = self.tree.resize_current_percent(target);
        let percent = percent_from_size_change(
            current_percent,
            available,
            || self.ppt_reference(target.child, layout),
            SizeChange::SetFixed(new_span),
        );

        self.tree.apply_resize(target, layout, percent)
    }

    pub fn new(
        view_size: Size<f64, Logical>,
        working_area: Rectangle<f64, Logical>,
        scale: f64,
        clock: Clock,
        options: Rc<Options>,
    ) -> Self {
        let tree = ContainerTree::new(view_size, working_area, scale, options.clone());
        let background_color = options.layout.background_color;

        Self {
            tree,
            view_size,
            working_area,
            viewport: FixedViewport,
            scale,
            clock,
            interactive_resize: None,
            options,
            tab_bar_cache: RefCell::new(HashMap::new()),
            is_active: false,
            floating_has_focus: false,
            closing_windows: Vec::new(),
            overview_offscreen: OffscreenBuffer::default(),
            overview_background: SolidColorBuffer::new(view_size, background_color),
        }
    }

    // Basic getters using ContainerTree
    pub fn windows(&self) -> impl Iterator<Item = &W> + '_ {
        self.tree.all_windows().into_iter()
    }

    pub fn tiles(&self) -> impl Iterator<Item = &Tile<W>> + '_ {
        self.tree.all_tiles().into_iter()
    }

    pub fn active_tile(&self) -> Option<&Tile<W>> {
        self.focused_tile()
    }

    pub fn active_window_mut(&mut self) -> Option<&mut W> {
        let key = self
            .tree
            .focus_inactive_view_in_branch(self.tree.workspace_root())?;
        Some(self.tree.get_tile_mut(key)?.window_mut())
    }

    pub fn is_active_pending_fullscreen(&self) -> bool {
        self.tree.fullscreen_key().is_some()
            || self
                .focused_tile()
                .is_some_and(|tile| tile.window().is_pending_windowed_fullscreen())
    }

    pub fn view_size(&self) -> Size<f64, Logical> {
        self.view_size
    }

    pub fn parent_area(&self) -> Rectangle<f64, Logical> {
        self.working_area
    }

    pub(super) fn working_area(&self) -> Rectangle<f64, Logical> {
        self.working_area
    }

    pub(super) fn scale(&self) -> f64 {
        self.scale
    }

    /// Whether one of the workspace's two sides is the focused one.
    pub(in crate::layout) fn side_is_active(&self, floating: bool) -> bool {
        self.is_active && self.floating_has_focus == floating
    }

    /// Whose the focus is: the workspace's, and then which of its two sides has it.
    ///
    /// The owner says, because only the owner knows — a side asked about itself would only
    /// ever answer yes.
    pub(super) fn set_active(&mut self, is_active: bool, floating_has_focus: bool) {
        self.is_active = is_active;
        self.floating_has_focus = floating_has_focus;
    }

    pub fn clock(&self) -> &Clock {
        &self.clock
    }

    pub fn options(&self) -> &Rc<Options> {
        &self.options
    }

    pub fn verify_invariants(&self) {
        self.tree.verify_invariants();
    }

    /// Whether any container in this space uses `layout`.
    ///
    /// Shape assertions use this instead of searching the debug dump for a layout name,
    /// which also matches window titles and changes meaning with the dump's format.
    #[cfg(test)]
    pub fn contains_layout(&self, layout: Layout) -> bool {
        self.tree.contains_layout(layout)
    }

    /// Window ids in visual (depth-first) order.
    #[cfg(test)]
    pub fn all_window_ids(&self) -> Vec<W::Id> {
        self.tree.all_window_ids()
    }

    /// Number of children directly under the tree root.
    #[cfg(test)]
    pub fn root_children_len(&self) -> usize {
        self.tree.root_children_len()
    }

    /// The focused window's id, for shape assertions that used to look for a `*` marker in
    /// the debug dump.
    #[cfg(test)]
    pub fn focused_window_id(&self) -> Option<W::Id> {
        self.tree.focused_window_id()
    }

    /// Whether the tree holds any container at all, as opposed to a lone window.
    #[cfg(test)]
    pub fn has_containers(&self) -> bool {
        [
            Layout::SplitH,
            Layout::SplitV,
            Layout::Tabbed,
            Layout::Stacked,
        ]
        .into_iter()
        .any(|layout| self.tree.contains_layout(layout))
    }

    #[cfg(test)]
    pub fn debug_tree(&self) -> String
    where
        W::Id: std::fmt::Display,
    {
        self.tree.debug_tree()
    }

    #[cfg(test)]
    pub fn focus_path(&self) -> Vec<usize> {
        self.tree.focus_path()
    }

    #[cfg(test)]
    pub fn debug_workspace_layout(&self) -> Layout {
        self.tree.workspace_layout()
    }

    #[cfg(test)]
    pub fn debug_root_is_workspace_node(&self) -> bool {
        self.tree.root_is_workspace_node()
    }

    pub fn selected_path(&self) -> Vec<usize> {
        self.tree.selected_path()
    }

    #[cfg(test)]
    pub fn debug_selection_visual_geometry(&self) -> Option<Rectangle<f64, Logical>> {
        self.selected_layout_parent_in_tiling()
            .then(|| self.selected_geometry())
            .flatten()
    }

    pub fn remove_window(&mut self, window: &W) -> Option<RemovedTile<W>> {
        let tile = self.tree.remove_window(window.id())?;

        // Create RemovedTile
        Some(RemovedTile {
            tile,
            width: ColumnWidth::default(),
            is_full_width: false,
            is_floating: false,
        })
    }

    pub fn update_window(&mut self, window: &W::Id, serial: Option<smithay::utils::Serial>) {
        let Some(key) = self.tiled_window_key(window) else {
            return;
        };
        let Some(tile) = self.tree.get_tile_mut(key) else {
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
        mut ctx: RenderCtx<R>,
        xray_pos: XrayPos,
        tiling_focus_ring: bool,
    ) -> Vec<TreeSpaceRenderElement<R>> {
        // Pre-allocate: ~4 elements per tile + closing windows + tab bars
        let tile_count = self.tree.window_count();
        let estimated_capacity = tile_count * 4 + self.closing_windows.len() + tile_count / 2;
        let mut elements = Vec::with_capacity(estimated_capacity);
        let mut active_elements = Vec::with_capacity(8);
        let scale = Scale::from(self.scale);
        let focused_key = self.focused_key();
        let selection_is_layout_parent = self.selected_layout_parent_in_tiling();
        let selection_is_active = self.side_is_active(false) || self.tree.workspace_is_selected();
        let fullscreen = self.fullscreen_render_state();
        let view_rect = Rectangle::from_size(self.view_size);

        for closing in self.closing_windows.iter().rev() {
            let elem = closing.render(ctx.as_gles(), view_rect, scale);
            elements.push(TreeSpaceRenderElement::ClosingWindow(elem));
        }

        // Render container selection before regular tiling elements so it ends up
        // visually on top after the global reverse-order composition pass.
        if selection_is_layout_parent && (tiling_focus_ring || selection_is_active) {
            if let Some(rect) = self.selected_geometry() {
                let mut selection_border = self.options.layout.border;
                if let Some(focus_info) = self
                    .display_layouts()
                    .find(|info| Some(info.key) == focused_key)
                {
                    if let Some(tile) = self.tree.get_tile(focus_info.key) {
                        if let Some(width) = tile.effective_border_width() {
                            selection_border.width = width;
                        }
                    }
                }
                render_container_selection(
                    ctx.renderer,
                    rect,
                    view_rect,
                    self.scale,
                    selection_is_active,
                    self.options.layout.focus_ring,
                    selection_border,
                    ContainerSelectionStyle::Tiling,
                    &mut |elem| elements.push(TreeSpaceRenderElement::ContainerSelection(elem)),
                );
            }
        }

        let render_layouts: Vec<&LeafLayoutInfo> = self.display_layouts().collect();
        for info in render_layouts.iter().rev() {
            // Use O(1) key lookup instead of O(depth) path lookup.
            if let Some(tile) = self.tree.get_tile(info.key) {
                let in_fullscreen_container = fullscreen
                    .container
                    .is_some_and(|scope| self.tree.is_descendant(info.key, scope));
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

                let is_focused = self.side_is_active(false)
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
                    target_elements.push(TreeSpaceRenderElement::from(elem));
                });
            }
        }

        elements.extend(active_elements);

        if !fullscreen.has_client_fullscreen() && !self.options.layout.tab_bar.off {
            let tab_bar_infos = self.tree.tab_bar_layouts();
            let mut cache = self.tab_bar_cache.borrow_mut();
            let gles = ctx.renderer.as_gles_renderer();
            let tab_bar_config = self.effective_tab_bar_config();
            let is_active_workspace = self.side_is_active(false);
            let target = ctx.target;
            for info in tab_bar_infos {
                if fullscreen
                    .container
                    .is_some_and(|scope| !self.tree.is_descendant(info.key, scope))
                {
                    continue;
                }
                let state = tab_bar_state_from_info(
                    &info,
                    &tab_bar_config,
                    is_active_workspace,
                    self.scale,
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
                        self.scale,
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
                elements.push(TreeSpaceRenderElement::TabBar(
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

    pub fn render<R: NiriRenderer>(
        &self,
        ctx: RenderCtx<R>,
        xray_pos: XrayPos,
        tiling_focus_ring: bool,
        push: &mut dyn FnMut(TreeSpaceRenderElement<R>),
    ) {
        for elem in self.render_elements(ctx, xray_pos, tiling_focus_ring) {
            push(elem);
        }
    }

    pub fn render_as_offscreen(
        &self,
        renderer: &mut GlesRenderer,
        target: RenderTarget,
        tiling_focus_ring: bool,
    ) -> Option<OffscreenRenderElement> {
        for tile in self.tiles() {
            tile.window().set_offscreen_data(None);
        }

        let ctx = RenderCtx {
            renderer,
            target,
            xray: None,
        };
        let mut elements = self.render_elements(ctx, XrayPos::default(), tiling_focus_ring);
        if elements.is_empty() {
            return None;
        }

        let background = SolidColorRenderElement::from_buffer(
            &self.overview_background,
            Point::from((0., 0.)),
            1.,
            Kind::Unspecified,
        );
        elements.push(background.into());

        self.overview_offscreen
            .render(renderer, Scale::from(self.scale), &elements)
            .map(|(elem, _sync, data)| {
                for tile in self.tiles() {
                    tile.window().set_offscreen_data(Some(data.clone()));
                }

                elem
            })
            .map_err(|err| warn!("error rendering tiling space to offscreen: {err:?}"))
            .ok()
    }

    // Layout operations using ContainerTree
    pub fn update_config(
        &mut self,
        view_size: Size<f64, Logical>,
        working_area: Rectangle<f64, Logical>,
        scale: f64,
        options: Rc<Options>,
    ) {
        self.view_size = view_size;
        self.working_area = working_area;
        self.scale = scale;
        self.options = options.clone();
        self.overview_background
            .update(view_size, options.layout.background_color);
        self.tree
            .update_config(view_size, working_area, scale, options);
        self.tree.layout();
    }

    pub fn set_view_size(
        &mut self,
        view_size: Size<f64, Logical>,
        working_area: Rectangle<f64, Logical>,
    ) {
        self.view_size = view_size;
        self.working_area = working_area;
        self.overview_background.resize(view_size);
        self.tree.set_view_size(view_size, working_area);
        // Recalculate layout on resize
        self.tree.layout();
    }

    pub fn advance_animations(&mut self) {
        for tile in self.tiles_mut() {
            tile.advance_animations();
        }

        self.closing_windows.retain_mut(|closing| {
            closing.advance_animations();
            closing.are_animations_ongoing()
        });
    }

    pub fn are_animations_ongoing(&self) -> bool {
        self.tiles().any(|tile| tile.are_animations_ongoing()) || !self.closing_windows.is_empty()
    }

    /// What a tile needs to know about the space it is in.
    ///
    /// Taken as a value so a caller can read it before borrowing the arena: a tile is reached
    /// through the tree, and asking the space about itself while holding one is the borrow
    /// checker's way of pointing out that these are two different questions.
    pub(super) fn tile_config(&self) -> TileConfig {
        TileConfig {
            view_size: self.view_size,
            scale: self.scale,
            options: self.options.clone(),
        }
    }

    pub(super) fn tab_bar_cache_mut(&self) -> RefMut<'_, HashMap<NodeKey, TabBarCacheEntry>> {
        self.tab_bar_cache.borrow_mut()
    }

    pub fn update_render_elements(&mut self) {
        let is_active = self.side_is_active(false);
        // Once a frame, and for both sides: a container that has left the tree will not be
        // asked for again, and its texture is the largest thing an entry holds.
        self.tab_bar_cache
            .borrow_mut()
            .retain(|key, _| self.tree.holds_node(*key));
        let applied = self.tree.apply_pending_layouts_if_ready();
        if applied && self.tree.take_pending_relayout() {
            self.tree.layout();
        }
        let has_pending = self.tree.has_pending_layouts();
        let mut state_layouts = if has_pending {
            self.tree
                .pending_leaf_layouts_cloned()
                .unwrap_or_else(|| self.tree.leaf_layouts_cloned())
        } else {
            self.tree.leaf_layouts_cloned()
        };
        let tiled_root = self.tree.workspace_root();
        state_layouts.retain(|info| info.branch == tiled_root);
        let workspace_view = Rectangle::from_size(self.view_size);
        let focused_key = self.focused_key();
        let selection_is_layout_parent = self.selected_layout_parent_in_tiling();
        let scale = Scale::from(self.scale);
        let fullscreen = self.fullscreen_render_state();
        let fullscreen_container = fullscreen.container;
        let logical_fullscreen_id = self.pending_fullscreen_window().cloned();
        let layout_rect = self.tree.layout_area();
        let is_single_window = self.tree.window_count() <= 1;
        // Clone here because we need mutable access to tree in the loop below.
        let render_layouts: Vec<LeafLayoutInfo> = self.display_layouts().cloned().collect();
        let render_edges: Vec<(FocusRingEdges, Option<FocusRingIndicatorEdge>)> = render_layouts
            .iter()
            .map(|info| {
                let edges = edge_visibility_for_tile(
                    &self.options,
                    layout_rect,
                    info.rect,
                    self.scale,
                    is_single_window,
                );
                let indicator_edge = split_indicator_edge_for_tile(&self.tree, info.key, edges);
                (edges, indicator_edge)
            })
            .collect();

        let ctx = WindowStateContext {
            focused_key,
            workspace_active: is_active,
            deactivate_unfocused: self.options.deactivate_unfocused_windows,
            request_size: !has_pending,
            working_area_size: self.working_area.size,
            options: &self.options,
            fullscreen_id: logical_fullscreen_id.as_ref(),
            windowed_fullscreen_id: fullscreen.windowed_fullscreen_id.as_ref(),
            has_fullscreen_container: fullscreen_container.is_some(),
            view_size: self.view_size,
        };
        // A stale snapshot describes a tree that no longer exists; driving window state
        // from it would flush configures carrying its obsolete bounds. The deferred
        // relayout will run this pass again once the transaction resolves.
        let skip_state_pass = self.tree.pending_layout_is_stale();
        let resize_data = self.interactive_resize_data_by_leaf(&state_layouts);
        for info in state_layouts {
            if skip_state_pass {
                break;
            }
            let in_fullscreen_container =
                fullscreen_container.is_some_and(|scope| self.tree.is_descendant(info.key, scope));
            // Use O(1) key lookup instead of O(depth) path lookup.
            if let Some(tile) = self.tree.get_tile_mut(info.key) {
                let resize = resize_data.get(&info.key).copied();
                Self::update_window_state(tile, &info, resize, in_fullscreen_container, &ctx);
            }
        }

        for (info, (edges, indicator_edge)) in render_layouts.into_iter().zip(render_edges) {
            let is_in_fullscreen_container =
                fullscreen_container.is_some_and(|scope| self.tree.is_descendant(info.key, scope));
            // Asked before the tile is borrowed mutably below.
            let is_focus_head = self.tree.is_focus_head(info.key);
            // Use O(1) key lookup instead of O(depth) path lookup.
            if let Some(tile) = self.tree.get_tile_mut(info.key) {
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
    }

    pub fn interactive_resize_begin(&mut self, window: W::Id, edges: ResizeEdge) -> bool {
        self.interactive_resize_begin_internal(window, edges, None)
    }

    pub fn interactive_resize_begin_at(
        &mut self,
        window: W::Id,
        edges: ResizeEdge,
        pos: Point<f64, Logical>,
    ) -> bool {
        self.interactive_resize_begin_internal(window, edges, Some(pos))
    }

    fn interactive_resize_begin_internal(
        &mut self,
        window: W::Id,
        edges: ResizeEdge,
        pos: Option<Point<f64, Logical>>,
    ) -> bool {
        if self.interactive_resize.is_some() {
            return false;
        }

        let Some((edges, horizontal, vertical)) =
            self.tree.resize_targets_for_window(&window, edges, pos)
        else {
            return false;
        };

        self.interactive_resize = Some(InteractiveResizeState {
            window,
            data: InteractiveResizeData { edges },
            horizontal,
            vertical,
        });
        self.tree.layout();
        true
    }

    pub fn interactive_resize_update(
        &mut self,
        window: &W::Id,
        delta: Point<f64, Logical>,
    ) -> bool {
        let Some(resize) = &self.interactive_resize else {
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
            self.tree.layout_with_animation_flags(false, false);
        }

        true
    }

    pub fn interactive_resize_end(&mut self, window: Option<&W::Id>) {
        let Some(resize) = &self.interactive_resize else {
            return;
        };

        if let Some(window) = window {
            if window != &resize.window {
                return;
            }
        }

        self.interactive_resize = None;
    }

    pub fn cancel_resize_for_window(&mut self, window: &W) {
        if self
            .interactive_resize
            .as_ref()
            .is_some_and(|resize| &resize.window == window.id())
        {
            self.interactive_resize = None;
        }
    }

    pub fn resize_edges_under(&mut self, pos: Point<f64, Logical>) -> Option<ResizeEdge> {
        self.resize_hit_under(pos).map(|hit| hit.edges)
    }

    pub fn resize_hit_under(&mut self, pos: Point<f64, Logical>) -> Option<ResizeHit<W::Id>> {
        let has_fullscreen_like = self.tiled_fullscreen_key().is_some()
            || self
                .tree
                .focused_tile()
                .is_some_and(|tile| tile.window().is_pending_windowed_fullscreen());
        if has_fullscreen_like {
            return None;
        }

        let (leaf_key, rect) = self.closest_leaf_rect(pos)?;
        let tile = self.tree.get_tile(leaf_key)?;
        if !tile.window().pending_sizing_mode().is_normal() {
            return None;
        }

        let border = tile.effective_border_width().unwrap_or(0.0) * 2.0;
        let threshold = super::RESIZE_EDGE_THRESHOLD.max(border);
        let gap_half = self.options.layout.gaps / 2.0;
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
            if !self.tree.has_resize_target(leaf_key, edge, layout, pos) {
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
    pub fn activate_window(&mut self, window: &W::Id) -> bool {
        self.tree.focus_window_by_id(window)
    }

    fn focus_in_direction_with_fullscreen_scope(
        &mut self,
        direction: Direction,
        allow_wrap: bool,
    ) -> bool {
        // A fullscreen node is the exact boundary for directional focus: focus can move among
        // descendants of a fullscreen container, but never inspect one of its siblings. A
        // fullscreen leaf is a one-node branch and therefore cannot move at all.
        let fullscreen_scope = self.tiled_fullscreen_key();

        if let Some(scope) = fullscreen_scope {
            self.tree
                .focus_in_direction_in_branch(scope, direction, false)
        } else if !allow_wrap {
            self.tree.focus_in_direction_no_wrap(direction)
        } else {
            self.tree.focus_in_direction(direction)
        }
    }

    pub fn focus_left(&mut self) -> bool {
        self.focus_in_direction_with_fullscreen_scope(Direction::Left, true)
    }

    pub fn focus_left_no_wrap(&mut self) -> bool {
        self.focus_in_direction_with_fullscreen_scope(Direction::Left, false)
    }

    pub fn focus_right(&mut self) -> bool {
        self.focus_in_direction_with_fullscreen_scope(Direction::Right, true)
    }

    pub fn focus_right_no_wrap(&mut self) -> bool {
        self.focus_in_direction_with_fullscreen_scope(Direction::Right, false)
    }

    pub fn focus_down(&mut self) -> bool {
        self.focus_in_direction_with_fullscreen_scope(Direction::Down, true)
    }

    pub fn focus_down_no_wrap(&mut self) -> bool {
        self.focus_in_direction_with_fullscreen_scope(Direction::Down, false)
    }

    pub fn focus_up(&mut self) -> bool {
        self.focus_in_direction_with_fullscreen_scope(Direction::Up, true)
    }

    pub fn focus_up_no_wrap(&mut self) -> bool {
        self.focus_in_direction_with_fullscreen_scope(Direction::Up, false)
    }

    /// sway's `focus next|prev [sibling]`.
    pub fn focus_along_parent(&mut self, forward: bool, descend: bool) -> bool {
        if self.has_fullscreen_window() {
            return false;
        }
        self.tree.focus_along_parent(forward, descend)
    }

    pub fn focus_parent(&mut self) -> bool {
        if let Some(fullscreen_key) = self.tiled_fullscreen_key() {
            // Focus may move inside the fullscreen subtree, including onto the container
            // that owns fullscreen, but it cannot climb through that owner. sway enforces
            // the fullscreen node as the focus boundary; treating any fullscreen presence
            // as a blanket no-op also blocked `focus parent` after a fullscreen leaf had
            // been wrapped by `split`.
            return self.tree.select_parent_in(fullscreen_key);
        }

        self.tree.select_parent()
    }

    /// Handle `focus parent` inside this branch's fullscreen boundary.
    ///
    /// The return value says the branch owns fullscreen, not whether focus moved: once the
    /// owner is reached, a no-op must still not be interpreted as reaching the workspace.
    pub(super) fn handle_focus_parent_in_branch_fullscreen_scope(
        &mut self,
        branch: NodeKey,
    ) -> bool {
        let Some(fullscreen) = self
            .tree
            .fullscreen_key()
            .filter(|key| self.tree.branch_root(*key) == branch)
        else {
            return false;
        };
        let _ = self.tree.select_parent_in(fullscreen);
        true
    }

    pub fn focus_child(&mut self) -> bool {
        self.tree.select_child()
    }

    pub fn focus_parent_targets_workspace(&self) -> bool {
        if self.has_fullscreen_window() {
            return false;
        }

        if self.selected_is_container() {
            return false;
        }

        if self.tree.selected_layout_parent_key() == Some(self.tree.workspace_root()) {
            return true;
        }

        self.tree.focused_leaf_targets_workspace_layout()
    }

    pub fn clear_selection_context(&mut self) {
        self.tree.clear_selection();
    }

    pub(super) fn clear_workspace_selection(&mut self) {
        if self.tree.workspace_is_selected() && !self.tree.is_empty() {
            self.tree.clear_selection();
        }
    }

    pub(super) fn root_layout_and_child_count(&self) -> Option<(Layout, usize)> {
        self.tree
            .container_info(self.tree.root_node_key()?)
            .map(|(layout, _rect, child_count)| (layout, child_count))
    }

    pub fn select_root_container(&mut self) -> bool {
        self.tree.select_root_container()
    }

    pub(super) fn inactive_tiling_key(&self) -> Option<NodeKey> {
        self.tree.inactive_tiling_key()
    }

    pub(super) fn inactive_tiling_reference(
        &self,
    ) -> Option<super::container::InactiveTilingReference> {
        self.tree.inactive_tiling_reference()
    }

    pub(super) fn inactive_floating_window_id(&self) -> Option<W::Id> {
        self.tree.inactive_floating_window_id()
    }

    pub(super) fn active_floating_window_id(&self) -> Option<W::Id> {
        self.tree.active_floating_window_id()
    }

    pub(super) fn focus_inactive_tiling_key(&mut self, key: NodeKey) -> bool {
        self.tree.focus_inactive_tiling_key(key)
    }

    pub(super) fn window_for_inactive_tiling_key(&self, key: NodeKey) -> Option<&W> {
        self.tree.window_for_inactive_tiling_key(key)
    }

    /// The branch the tiled commands act on: sway's `ws->tiling`.
    fn tiled_branch(&self) -> NodeKey {
        self.tree.workspace_root()
    }

    fn move_command_target(&mut self, direction: Direction) -> bool {
        // sway's `container_move_in_direction` opens by refusing to move a fullscreen
        // container within its workspace: one fullscreen on the workspace considers outputs
        // and nothing else, one fullscreen globally does not move at all. Neither of them
        // ever looks at the tree, so nothing below applies. The fullscreen is the tiled
        // side's to know about, which is why this guard is here and not in `move_in_branch`.
        let target = self.tree.command_target_in(self.tiled_branch());
        if self.target_is_fullscreen(target) {
            return false;
        }
        self.move_in_branch(self.tiled_branch(), direction)
    }

    /// Move whatever a command in this branch is aimed at, and arrange if it moved.
    pub(super) fn move_in_branch(&mut self, branch: NodeKey, direction: Direction) -> bool {
        let target = self.tree.command_target_in(branch);
        self.mutate_tree(|tree| tree.move_target_in_direction(direction, target))
    }

    fn target_is_fullscreen(&self, target: NodeKey) -> bool {
        if target == self.tree.workspace_root() {
            return false;
        }
        self.tiled_fullscreen_key()
            .is_some_and(|fullscreen| self.tree.is_descendant(target, fullscreen))
    }

    // Move operations using ContainerTree
    pub fn move_left(&mut self) -> bool {
        self.move_command_target(Direction::Left)
    }

    pub fn move_right(&mut self) -> bool {
        self.move_command_target(Direction::Right)
    }

    pub fn move_down(&mut self) -> bool {
        self.move_command_target(Direction::Down)
    }

    pub fn move_up(&mut self) -> bool {
        self.move_command_target(Direction::Up)
    }

    // Container operations (replacing column operations)
    pub fn consume_into_column(&mut self) {
        // In i3 model: create vertical split
        self.split_in_branch(self.tiled_branch(), Layout::SplitV);
    }

    pub fn expel_from_column(&mut self) {
        // In i3 model: create horizontal split
        self.split_in_branch(self.tiled_branch(), Layout::SplitH);
    }

    /// Split focused window horizontally (i3-style)
    pub fn split_horizontal(&mut self) {
        self.split_in_branch(self.tiled_branch(), Layout::SplitH);
    }

    /// Split focused window vertically (i3-style)
    pub fn split_vertical(&mut self) {
        self.split_in_branch(self.tiled_branch(), Layout::SplitV);
    }

    /// Split workspace root like workspace root split.
    pub fn split_workspace_horizontal(&mut self) {
        self.split_workspace(Layout::SplitH);
    }

    /// Split workspace root like workspace root split.
    pub fn split_workspace_vertical(&mut self) {
        self.split_workspace(Layout::SplitV);
    }

    /// `split` with the workspace itself selected. The rule lives on the tree; this is the
    /// space's relayout contract around it.
    fn split_workspace(&mut self, layout: Layout) {
        self.mutate_tree(|tree| tree.split_workspace_container(layout));
    }

    /// Set layout mode for focused container
    pub fn set_layout_mode(&mut self, layout: Layout) {
        self.set_layout_in_branch(self.tiled_branch(), layout);
    }

    pub fn split_none(&mut self) {
        self.unsplit_in_branch(self.tiled_branch());
    }

    /// `layout` with the workspace itself selected.
    ///
    /// Measured against sway 1.11: this never builds a container. The workspace node carries
    /// the orientation itself, so an empty workspace and a lone window are the same case.
    pub fn set_workspace_layout_mode(&mut self, layout: Layout) -> bool {
        self.mutate_tree(|tree| tree.set_root_layout_from_command(layout))
    }

    /// The layout of the parent of whatever a command is aimed at.
    ///
    /// sway's `container_parent_layout(config->handler_context.container)`. `None` when the
    /// workspace itself is what is selected: sway has no container to ask there, because a
    /// workspace is not one.
    pub fn command_target_parent_layout(&self) -> Option<Layout> {
        let key = self.tree.selected_node_key()?;
        if Some(key) == self.tree.root_node_key() {
            return None;
        }
        self.tree.parent_layout(key)
    }

    /// Toggle between horizontal and vertical split for the focused container.
    pub fn toggle_split_layout(&mut self) {
        self.toggle_split_in_branch(self.tiled_branch());
    }

    pub fn toggle_workspace_split_layout(&mut self) {
        let next = self
            .tree
            .toggled_split_layout(self.tree.workspace_layout(), self.tree.root_node_key());
        self.set_workspace_layout_mode(next);
    }

    /// Cycle focused container layout in sway-style order.
    pub fn toggle_layout_all(&mut self) {
        self.toggle_layout_all_in_branch(self.tiled_branch());
    }

    pub fn toggle_workspace_layout_all(&mut self) {
        let next = self.tree.workspace_layout().next_in_cycle();
        self.set_workspace_layout_mode(next);
    }

    pub fn set_default_layout(&mut self) {
        self.set_default_layout_in_branch(self.tiled_branch());
    }

    pub fn set_workspace_default_layout(&mut self) {
        self.mutate_tree(|tree| {
            let root = tree.workspace_root();
            tree.set_default_layout_for_target(root)
        });
    }

    pub(super) fn toggle_layout_cycle(&mut self, cycle: &[LayoutCycleEntry]) {
        self.toggle_layout_cycle_in_branch(self.tiled_branch(), cycle);
    }

    pub(super) fn toggle_workspace_layout_cycle(&mut self, cycle: &[LayoutCycleEntry]) {
        self.mutate_tree(|tree| {
            let root = tree.workspace_root();
            tree.toggle_layout_cycle_for_target(root, cycle)
        });
    }

    /// Set the width of the currently focused root-level column
    pub fn set_column_width(&mut self, change: SizeChange) {
        let Some(idx) = self.tree.focused_root_index() else {
            return;
        };
        let Some(root_key) = self.tree.root_node_key() else {
            return;
        };

        let Some((layout, rect, child_count)) = self.tree.container_info(root_key) else {
            return;
        };
        if layout != Layout::SplitH || child_count == 0 {
            return;
        }

        let available_width = self.available_span(rect.size.w, child_count);
        if available_width <= 0.0 {
            return;
        }

        let current_percent = self.tree.child_percent(root_key, idx).unwrap_or(1.0);
        let new_percent =
            percent_from_size_change(current_percent, available_width, || rect.size.w, change);

        if self
            .tree
            .set_child_percent(root_key, idx, Layout::SplitH, new_percent)
        {
            self.tree.layout();
        }
    }
    pub fn reset_window_height(&mut self, window: Option<&W::Id>) {
        let Some(key) = self.window_target(window) else {
            return;
        };

        let Some((parent_key, _, _, _child_count, _rect)) =
            self.window_container_metrics(key, Layout::SplitV)
        else {
            return;
        };

        if self
            .tree
            .container_info(parent_key)
            .is_some_and(|(layout, _, _)| layout == Layout::SplitV)
        {
            self.tree.recalculate_child_percents(parent_key);
            self.tree.layout();
        }
    }

    /// Toggle fullscreen state for a window
    pub fn toggle_fullscreen(&mut self, window: &W) {
        let currently = self.is_fullscreen(window);
        let _ = self.set_fullscreen(window.id(), !currently);
    }
    pub fn toggle_width(&mut self, forwards: bool) {
        let Some(idx) = self.tree.focused_root_index() else {
            return;
        };
        let Some(root_key) = self.tree.root_node_key() else {
            return;
        };

        let Some((layout, rect, child_count)) = self.tree.container_info(root_key) else {
            return;
        };
        if layout != Layout::SplitH || child_count == 0 {
            return;
        }

        let available = self.available_span(rect.size.w, child_count);
        if available <= 0.0 {
            return;
        }

        let current_percent = self.tree.child_percent(root_key, idx).unwrap_or(1.0);
        let presets = &self.options.layout.preset_column_widths;

        if let Some(percent) = self.cycle_presets(available, current_percent, presets, forwards) {
            if self
                .tree
                .set_child_percent(root_key, idx, Layout::SplitH, percent)
            {
                self.tree.layout();
            }
        }
    }

    #[cfg(test)]
    pub fn view_pos(&self) -> f64 {
        self.viewport.position()
    }

    #[cfg(test)]
    pub fn active_column_idx(&self) -> usize {
        self.tree.focused_root_index().unwrap_or(0)
    }

    fn layout_area(&self) -> Rectangle<f64, Logical> {
        let mut area = self.working_area;
        let gap = self.options.layout.gaps;
        if gap > 0.0 {
            area.loc.x += gap;
            area.loc.y += gap;
            area.size.w = (area.size.w - gap * 2.0).max(0.0);
            area.size.h = (area.size.h - gap * 2.0).max(0.0);
        }
        area
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

    fn leaf_rect_for_key(&self, key: NodeKey) -> Option<Rectangle<f64, Logical>> {
        let scale = Scale::from(self.scale);
        let info = self.display_layouts().find(|info| info.key == key)?;
        let tile = self.tree.get_tile(info.key)?;
        let mut tile_pos = info.rect.loc + tile.render_offset();
        tile_pos = tile_pos.to_physical_precise_round(scale).to_logical(scale);
        Some(Rectangle::new(tile_pos, tile.tile_size()))
    }

    /// The leaf under `pos`, or the nearest one: its stable identity and on-screen rect.
    fn closest_leaf_rect(&self, pos: Point<f64, Logical>) -> Option<LeafHit> {
        let scale = Scale::from(self.scale);
        let fullscreen = self.fullscreen_render_state();

        let mut nearest: Option<(LeafHit, f64)> = None;

        for info in self.display_layouts() {
            if let Some(tile) = self.tree.get_tile(info.key) {
                let in_fullscreen_container = fullscreen
                    .container
                    .is_some_and(|scope| self.tree.is_descendant(info.key, scope));
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
    pub(super) fn insert_position(&self, pos: Point<f64, Logical>) -> InsertPosition {
        if self.tree.is_empty() {
            return InsertPosition::NewColumn(0);
        }

        let layout_area = self.layout_area();
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

        let Some((leaf_key, rect)) = self.closest_leaf_rect(pos) else {
            return InsertPosition::NewColumn(0);
        };

        let parent_layout = self.tree.parent_layout(leaf_key).unwrap_or(Layout::SplitH);

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
        position: &InsertPosition,
    ) -> Option<Rectangle<f64, Logical>> {
        match position {
            InsertPosition::NewColumn(_) => Some(self.layout_area()),
            InsertPosition::Swap { target, .. } => {
                let rect = self.leaf_rect_for_key(*target)?;
                let thickness = f64::min(rect.size.w, rect.size.h) * Self::DROP_CENTER_RATIO;
                Some(Self::inset_rect(rect, thickness))
            }
            InsertPosition::Split {
                target,
                direction,
                indicator,
            } => {
                let rect = self.leaf_rect_for_key(*target)?;
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
                let rect = self.layout_area();
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
    fn tab_bar_hit(&self, pos: Point<f64, Logical>) -> Option<(&W, super::HitType)> {
        if self.render_fullscreen_window().is_some() {
            return None;
        }
        self.branch_tab_bar_hit(self.tiled_branch(), pos, 0)
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
        if self.options.layout.tab_bar.off {
            return None;
        }

        let gap = self.branch_gap(branch);
        let fullscreen_container = (branch == self.tiled_branch())
            .then(|| self.render_fullscreen_container())
            .flatten();
        let cache = self.tab_bar_cache.borrow();
        for mut info in self.tree.tab_bar_layouts_in_branch(branch) {
            if fullscreen_container.is_some_and(|scope| !self.tree.is_descendant(info.key, scope)) {
                continue;
            }
            // A branch's outermost bar sits on the branch's edge, and the gap is outside it.
            if gap > 0.0 && info.path.is_empty() {
                info.rect.loc.x -= gap;
                info.rect.loc.y -= gap;
                info.rect.size.w = (info.rect.size.w + gap * 2.0).max(0.0);
            }
            let cached_widths = cache
                .get(&info.key)
                .map(|entry| entry.tab_widths_px.as_slice());
            let Some(tab_idx) = tab_bar_hit_index(&info, pos, self.scale, cached_widths, pad)
            else {
                continue;
            };

            if let Some(window) = self.tree.window_for_tab(info.key, tab_idx) {
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

    pub fn window_under(&self, pos: Point<f64, Logical>) -> Option<(&W, super::HitType)> {
        let scale = Scale::from(self.scale);
        let fullscreen = self.fullscreen_render_state();

        if let Some(hit) = self.tab_bar_hit(pos) {
            return Some(hit);
        }

        let render_layouts: Vec<&LeafLayoutInfo> = self.display_layouts().collect();
        for info in render_layouts.iter().rev() {
            // Use O(1) key lookup instead of O(depth) path lookup.
            if let Some(tile) = self.tree.get_tile(info.key) {
                let in_fullscreen_container = fullscreen
                    .container
                    .is_some_and(|scope| self.tree.is_descendant(info.key, scope));
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

    pub fn window_loc(&self, window: &W) -> Option<Point<f64, Logical>> {
        let key = self.tiled_window_key(window.id())?;
        let info = self.display_layouts().find(|layout| layout.key == key)?;
        let tile = self.tree.get_tile(key)?;
        let scale = Scale::from(self.scale);

        let mut tile_pos = info.rect.loc + tile.render_offset();
        tile_pos = tile_pos.to_physical_precise_round(scale).to_logical(scale);

        Some(tile_pos + tile.window_loc())
    }

    pub fn window_size(&self, window: &W) -> Option<Size<f64, Logical>> {
        let key = self.tiled_window_key(window.id())?;
        let tile = self.tree.get_tile(key)?;
        Some(tile.window_size())
    }

    pub fn is_fullscreen(&self, window: &W) -> bool {
        self.tiled_window_key(window.id()).is_some()
            && self.tree.window_owns_fullscreen(window.id())
    }

    pub fn has_fullscreen_window(&self) -> bool {
        self.tiled_fullscreen_key().is_some()
    }

    /// Set the display mode for the focused container
    pub fn set_column_display(&mut self, display: ColumnDisplay) {
        let layout = match display {
            ColumnDisplay::Normal => Layout::SplitV,
            ColumnDisplay::Tabbed => Layout::Tabbed,
        };

        self.set_layout_in_branch(self.tiled_branch(), layout);
    }

    /// Toggle between tabbed and normal (split) layout for focused container
    pub fn toggle_column_tabbed_display(&mut self) {
        let current = self.selection_layout_in(self.tiled_branch());
        let target = match current {
            Some(Layout::Tabbed) => Layout::SplitV,
            _ => Layout::Tabbed,
        };

        self.set_layout_in_branch(self.tiled_branch(), target);
    }

    // Additional methods needed by workspace.rs
    pub fn tiles_mut(&mut self) -> impl Iterator<Item = &mut Tile<W>> + '_ {
        let root = self.tree.workspace_root();
        self.tree.tiles_in_branch_mut(root).into_iter()
    }

    pub fn tiles_with_render_positions(
        &self,
    ) -> impl Iterator<Item = (&Tile<W>, Point<f64, Logical>, bool)> + '_ {
        let scale = Scale::from(self.scale);
        let fullscreen_container = self.render_fullscreen_container();
        self.display_layouts().filter_map(move |info| {
            // Use O(1) key lookup instead of O(depth) path lookup.
            let tile = self.tree.get_tile(info.key)?;
            let pos = info.rect.loc + tile.render_offset();
            let pos = pos.to_physical_precise_round(scale).to_logical(scale);
            let visible = info.visible
                && fullscreen_container
                    .is_none_or(|scope| self.tree.is_descendant(info.key, scope));
            Some((tile, pos, visible))
        })
    }

    pub fn tiles_with_render_positions_mut(
        &mut self,
        round: bool,
    ) -> impl Iterator<Item = (&mut Tile<W>, Point<f64, Logical>)> + '_ {
        let scale = Scale::from(self.scale);
        let layouts: Vec<LeafLayoutInfo> = self.display_layouts().cloned().collect();
        let keys: Vec<NodeKey> = layouts.iter().map(|info| info.key).collect();
        let locs: Vec<Point<f64, Logical>> = layouts.iter().map(|info| info.rect.loc).collect();
        self.tree
            .tiles_mut_for_keys(&keys)
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
    ) -> impl Iterator<Item = (&Tile<W>, tiri_ipc::WindowLayout)> + '_ {
        let scale = Scale::from(self.scale);
        let legacy_positions = self.legacy_tiling_positions();

        self.display_layouts().filter_map(move |info| {
            let tile = self.tree.get_tile(info.key)?;
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
            layout.pos_in_tiling_layout = legacy_positions.get(&info.path).copied();
            Some((tile, layout))
        })
    }

    fn legacy_tiling_positions(&self) -> HashMap<Vec<usize>, (usize, usize)> {
        let mut positions = HashMap::new();

        if self.tree.root_children_len() == 0 {
            return positions;
        }

        if self.tree.root_container().is_none() {
            positions.insert(Vec::new(), (1, 1));
            return positions;
        }

        for root_idx in 0..self.tree.root_children_len() {
            for (leaf_idx, path) in self
                .tree
                .leaf_paths_under(&[root_idx])
                .into_iter()
                .enumerate()
            {
                positions.insert(path, (root_idx + 1, leaf_idx + 1));
            }
        }

        positions
    }

    pub fn are_transitions_ongoing(&self) -> bool {
        self.tiles().any(|tile| tile.are_transitions_ongoing()) || !self.closing_windows.is_empty()
    }

    pub fn update_shaders(&mut self) {
        for tile in self.tiles_mut() {
            tile.update_shaders();
        }
    }

    pub fn active_window(&self) -> Option<&W> {
        self.tree
            .focused_window_in_branch(self.tree.workspace_root())
    }

    pub fn is_empty(&self) -> bool {
        self.tree.is_empty()
    }

    pub fn add_tile(
        &mut self,
        col_idx: Option<usize>,
        tile: Tile<W>,
        activate: bool,
        _width: ColumnWidth,
        _is_full_width: bool,
        _height: Option<WindowHeight>,
    ) {
        let id = tile.window().id().clone();
        if let Some(index) = col_idx {
            self.tree.insert_leaf_at(index, tile, activate);
        } else {
            self.tree.insert_window_with_focus(tile, activate);
        }
        self.sync_fullscreen_window();
        self.layout_after_tiled_insert(&id);
    }

    pub fn add_tile_right_of(
        &mut self,
        next_to: &W::Id,
        tile: Tile<W>,
        activate: bool,
        _width: ColumnWidth,
        _is_full_width: bool,
    ) {
        let id = tile.window().id().clone();
        self.tree.insert_leaf_after(next_to, tile, activate);
        self.sync_fullscreen_window();
        self.layout_after_tiled_insert(&id);
    }

    pub fn add_tile_to_root_container(
        &mut self,
        root_idx: usize,
        tile_idx: Option<usize>,
        tile: Tile<W>,
        activate: bool,
    ) {
        let id = tile.window().id().clone();
        if self
            .tree
            .insert_leaf_in_root_container(root_idx, tile_idx, tile, activate)
        {
            self.sync_fullscreen_window();
            self.layout_after_tiled_insert(&id);
        }
    }

    pub fn add_tile_to_column(
        &mut self,
        col_idx: usize,
        tile_idx: Option<usize>,
        tile: Tile<W>,
        activate: bool,
    ) {
        self.add_tile_to_root_container(col_idx, tile_idx, tile, activate);
    }

    pub fn insert_subtree_at_root(&mut self, index: usize, subtree: DetachedNode<W>, focus: bool) {
        self.mutate_tree(|tree| tree.insert_subtree_at_root(index, subtree, focus));
    }

    pub fn insert_subtree_with_focus(&mut self, subtree: DetachedNode<W>, focus: bool) {
        self.tree.insert_subtree_with_focus(subtree, focus);
        self.sync_fullscreen_window();
        self.tree.layout();
    }

    pub fn add_subtree_as_workspace_tiling_fallback(
        &mut self,
        subtree: DetachedNode<W>,
        focus: bool,
    ) {
        if self.tree.is_empty() {
            self.tree.insert_subtree_with_focus(subtree, focus);
        } else {
            let index = self.tree.root_children_len();
            self.tree.insert_subtree_at_root(index, subtree, focus);
        }
        self.sync_fullscreen_window();
        self.tree.layout();
    }

    pub fn add_tile_as_workspace_tiling_fallback(&mut self, tile: Tile<W>, activate: bool) {
        let id = tile.window().id().clone();
        if self.tree.is_empty() {
            self.tree.insert_window_with_focus(tile, activate);
        } else {
            let index = self.tree.root_children_len();
            self.tree.insert_leaf_at(index, tile, activate);
        }
        self.sync_fullscreen_window();
        self.layout_after_tiled_insert(&id);
    }

    pub(super) fn insert_parent_info_for_window(
        &self,
        window: &W::Id,
    ) -> Option<super::container::InsertParentInfo> {
        self.tree.insert_parent_info_for_window(window)
    }

    pub(super) fn replace_tiling_tile(&mut self, key: NodeKey, tile: Tile<W>) -> Option<Tile<W>> {
        if !self.is_tiling_leaf(key) {
            return None;
        }
        self.tree.replace_leaf(key, tile)
    }

    pub(super) fn is_tiling_leaf(&self, key: NodeKey) -> bool {
        self.tree.is_leaf(key) && self.tree.branch_root(key) == self.tree.workspace_root()
    }

    pub(super) fn insert_tile_with_parent_info(
        &mut self,
        info: &super::container::InsertParentInfo,
        tile: Tile<W>,
        activate: bool,
    ) -> bool {
        let root = self.tree.workspace_root();
        if self
            .tree
            .insert_leaf_with_parent_info(root, info, tile, activate)
        {
            self.sync_fullscreen_window();
            self.tree.layout();
            return true;
        }

        false
    }

    /// Split-insert next to the live tiling leaf selected by the drag-and-drop hit result.
    pub fn insert_tile_split(
        &mut self,
        target: NodeKey,
        direction: Direction,
        tile: Tile<W>,
        activate: bool,
    ) -> bool {
        if !self.is_tiling_leaf(target) {
            return false;
        }
        if self
            .tree
            .insert_leaf_split(target, direction, tile, activate)
        {
            self.sync_fullscreen_window();
            self.tree.layout();
            return true;
        }

        false
    }

    pub fn insert_tile_split_root(
        &mut self,
        direction: Direction,
        tile: Tile<W>,
        activate: bool,
    ) -> bool {
        if self.tree.insert_leaf_split_root(direction, tile, activate) {
            self.sync_fullscreen_window();
            self.tree.layout();
            return true;
        }

        false
    }

    pub fn active_tile_visual_rectangle(&self) -> Option<Rectangle<f64, Logical>> {
        let focused_key = self.focused_key();
        self.tree
            .leaf_layouts()
            .iter()
            .find(|info| Some(info.key) == focused_key)
            .and_then(|info| {
                let mut rect = info.rect;
                let tile = self.tree.get_tile(info.key)?;
                rect.loc += tile.render_offset();
                Some(rect)
            })
    }

    /// Get mutable reference to the currently focused tile
    pub fn active_tile_mut(&mut self) -> Option<&mut Tile<W>> {
        let key = self
            .tree
            .focus_inactive_view_in_branch(self.tree.workspace_root())?;
        self.tree.get_tile_mut(key)
    }

    pub fn add_root_tiling_subtree(
        &mut self,
        root_idx: Option<usize>,
        subtree: RootTilingSubtree<W>,
        activate: bool,
        _height: Option<WindowHeight>,
    ) {
        let idx = root_idx.unwrap_or_else(|| self.tree.root_children_len());
        self.tree
            .insert_subtree_at_root(idx, subtree.into_subtree(), activate);
        self.sync_fullscreen_window();
        self.tree.layout();
    }

    pub fn add_column(
        &mut self,
        col_idx: Option<usize>,
        column: Column<W>,
        activate: bool,
        height: Option<WindowHeight>,
    ) {
        self.add_root_tiling_subtree(col_idx, column.into(), activate, height);
    }
    pub fn remove_tile(&mut self, window: &W::Id, transaction: Transaction) -> RemovedTile<W> {
        self.tree.set_pending_transaction(transaction.clone());
        let tile = self
            .tree
            .remove_window(window)
            .expect("attempted to remove missing window");

        RemovedTile {
            tile,
            width: ColumnWidth::default(),
            is_full_width: false,
            is_floating: false,
        }
    }
    pub fn remove_active_tile(&mut self, transaction: Transaction) -> Option<RemovedTile<W>> {
        let id = self.focused_tile()?.window().id().clone();
        Some(self.remove_tile(&id, transaction))
    }
    pub fn remove_active_root_tiling_subtree(&mut self) -> Option<RootTilingSubtree<W>> {
        let idx = self.tree.focused_root_index()?;
        let subtree = self.tree.take_root_child_subtree(idx)?;
        let subtree = RootTilingSubtree::from_subtree(subtree);

        self.tree.layout();
        Some(subtree)
    }

    /// Remove the leaf or explicitly selected parent addressed by `move container`.
    pub fn remove_active_tiling_subtree(&mut self) -> Option<RootTilingSubtree<W>> {
        let subtree = self.tree.take_command_target_subtree()?;
        let subtree = RootTilingSubtree::from_subtree(subtree);

        self.tree.layout();
        Some(subtree)
    }

    pub fn remove_active_column(&mut self) -> Option<Column<W>> {
        self.remove_active_root_tiling_subtree().map(Into::into)
    }

    pub fn new_window_size(
        &self,
        _width: Option<PresetSize>,
        _height: Option<PresetSize>,
        rules: &ResolvedWindowRules,
    ) -> Size<i32, Logical> {
        let Some(preview) = self.tree.preview_new_leaf_geometry() else {
            return Size::from((800, 600));
        };

        let mut size = preview.rect.size;
        let mut border_config = self.options.layout.border.merged_with(&rules.border);
        border_config.width = round_logical_in_physical_max1(self.scale, border_config.width);

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

    pub fn focus_root_container_first(&mut self) {
        self.tree.focus_root_child(0);
    }

    pub fn focus_first_leaf(&mut self) {
        let _ = self.tree.focus_leaf_in_root_child(0, 1) || self.tree.focus_root_child(0);
    }

    pub fn focus_root_container_last(&mut self) {
        let len = self.tree.root_children_len();
        if len > 0 {
            self.tree.focus_root_child(len - 1);
        }
    }

    /// Root containers are 1-based to match user-facing commands.
    pub fn focus_root_container(&mut self, idx: usize) {
        if idx == 0 {
            return;
        }
        self.tree.focus_root_child(idx - 1);
    }

    /// Leaves inside the current root container are 1-based.
    pub fn focus_leaf_in_root_container(&mut self, index: u8) {
        if index == 0 {
            return;
        }
        let root_idx = match self.tree.focused_root_index() {
            Some(idx) => idx,
            None => return,
        };
        self.tree.focus_leaf_in_root_child(root_idx, index as usize);
    }

    pub fn focus_column_first(&mut self) {
        self.focus_root_container_first();
    }

    pub fn focus_column_last(&mut self) {
        self.focus_root_container_last();
    }

    pub fn focus_column(&mut self, idx: usize) {
        self.focus_root_container(idx);
    }

    pub fn focus_window_in_column(&mut self, index: u8) {
        self.focus_leaf_in_root_container(index);
    }

    pub fn focus_down_or_left(&mut self) {
        let _ = self.tree.focus_in_direction(Direction::Down)
            || self.tree.focus_in_direction(Direction::Left);
    }

    pub fn focus_down_or_right(&mut self) {
        let _ = self.tree.focus_in_direction(Direction::Down)
            || self.tree.focus_in_direction(Direction::Right);
    }

    pub fn focus_up_or_left(&mut self) {
        let _ = self.tree.focus_in_direction(Direction::Up)
            || self.tree.focus_in_direction(Direction::Left);
    }

    pub fn focus_up_or_right(&mut self) {
        let _ = self.tree.focus_in_direction(Direction::Up)
            || self.tree.focus_in_direction(Direction::Right);
    }

    pub fn focus_top(&mut self) {
        self.tree.focus_first_leaf_in_focused_root_child();
    }

    pub fn focus_bottom(&mut self) {
        self.tree.focus_last_leaf_in_focused_root_child();
    }

    fn move_root_child_with_layout(&mut self, current: usize, target: usize) -> bool {
        if current == target {
            return false;
        }
        if target >= self.tree.root_children_len() {
            return false;
        }
        self.mutate_tree(|tree| tree.move_root_child(current, target))
    }

    pub fn move_root_container_to_first(&mut self) {
        if let Some(idx) = self.tree.focused_root_index() {
            self.move_root_child_with_layout(idx, 0);
        }
    }

    pub fn move_root_container_to_last(&mut self) {
        let len = self.tree.root_children_len();
        if len == 0 {
            return;
        }
        if let Some(idx) = self.tree.focused_root_index() {
            self.move_root_child_with_layout(idx, len - 1);
        }
    }

    pub fn move_root_container_left(&mut self) -> bool {
        let Some(idx) = self.tree.focused_root_index() else {
            return false;
        };
        if idx == 0 {
            return false;
        }

        self.move_root_child_with_layout(idx, idx - 1)
    }

    pub fn move_root_container_right(&mut self) -> bool {
        let Some(idx) = self.tree.focused_root_index() else {
            return false;
        };
        let len = self.tree.root_children_len();
        if idx + 1 >= len {
            return false;
        }

        self.move_root_child_with_layout(idx, idx + 1)
    }

    pub fn move_root_container_to_index(&mut self, idx: usize) {
        if idx == 0 {
            return;
        }
        let target = idx - 1;
        if let Some(current) = self.tree.focused_root_index() {
            if current == target {
                return;
            }
            self.move_root_child_with_layout(current, target);
        }
    }

    pub fn move_column_to_first(&mut self) {
        self.move_root_container_to_first();
    }

    pub fn move_column_to_last(&mut self) {
        self.move_root_container_to_last();
    }

    pub fn move_column_left(&mut self) -> bool {
        self.move_root_container_left()
    }

    pub fn move_column_right(&mut self) -> bool {
        self.move_root_container_right()
    }

    pub fn move_column_to_index(&mut self, idx: usize) {
        self.move_root_container_to_index(idx);
    }

    fn consume_or_expel_window(&mut self, window: Option<&W::Id>, direction: Direction) {
        if let Some(id) = window {
            self.tree.focus_window_by_id(id);
        }

        if !self.move_command_target(direction) {
            self.mutate_tree(|tree| tree.split_focused(Layout::SplitV));
        }
    }

    pub fn consume_or_expel_window_left(&mut self, window: Option<&W::Id>) {
        self.consume_or_expel_window(window, Direction::Left);
    }

    pub fn consume_or_expel_window_right(&mut self, window: Option<&W::Id>) {
        self.consume_or_expel_window(window, Direction::Right);
    }

    pub fn toggle_full_width(&mut self) {
        let Some(tile) = self.focused_tile() else {
            return;
        };
        let id = tile.window().id().clone();
        let currently_fullscreen = self.tree.window_owns_fullscreen(tile.window().id());
        let _ = self.set_fullscreen(&id, !currently_fullscreen);
    }

    fn toggle_window_dimension(
        &mut self,
        window: Option<&W::Id>,
        layout: Layout,
        presets: &[PresetSize],
        forwards: bool,
    ) {
        let Some(key) = self.window_target(window) else {
            return;
        };
        let Some((parent_key, child_idx, available, _, _)) =
            self.window_container_metrics(key, layout)
        else {
            return;
        };
        let current_percent = self
            .tree
            .child_percent(parent_key, child_idx)
            .unwrap_or(1.0);

        if let Some(percent) = self.cycle_presets(available, current_percent, presets, forwards) {
            if self
                .tree
                .set_child_percent(parent_key, child_idx, layout, percent)
            {
                self.tree.layout();
            }
        }
    }

    pub fn toggle_window_height(&mut self, window: Option<&W::Id>, forwards: bool) {
        let presets = self.options.layout.preset_window_heights.clone();
        self.toggle_window_dimension(window, Layout::SplitV, &presets, forwards);
    }

    pub fn toggle_window_width(&mut self, window: Option<&W::Id>, forwards: bool) {
        let presets = self.options.layout.preset_column_widths.clone();
        self.toggle_window_dimension(window, Layout::SplitH, &presets, forwards);
    }

    pub fn set_window_width(&mut self, window: Option<&W::Id>, change: SizeChange) {
        self.resize_window(
            window,
            ResizeRequest::Axis {
                axis: ResizeAxis::Horizontal,
                change,
            },
        );
    }

    pub fn set_window_height(&mut self, window: Option<&W::Id>, change: SizeChange) {
        self.resize_window(
            window,
            ResizeRequest::Axis {
                axis: ResizeAxis::Vertical,
                change,
            },
        );
    }

    /// Apply one semantic resize request to a tiled target.
    pub fn resize_window(&mut self, window: Option<&W::Id>, request: ResizeRequest) {
        let (change, layout, reach) = match request {
            ResizeRequest::Axis {
                axis: ResizeAxis::Horizontal,
                change,
            } => (change, Layout::SplitH, ResizeReach::Siblings),
            ResizeRequest::Axis {
                axis: ResizeAxis::Vertical,
                change,
            } => (change, Layout::SplitV, ResizeReach::Siblings),
            ResizeRequest::Edge { direction, amount } => {
                let reach = if direction.is_leading() {
                    ResizeReach::Before
                } else {
                    ResizeReach::After
                };
                (
                    SizeChange::AdjustFixed(amount),
                    direction.split_layout(),
                    reach,
                )
            }
        };
        self.resize_window_with_reach(window, change, layout, reach);
    }

    /// Resize a window's share within its nearest ancestor split along `layout`'s axis.
    ///
    /// sway's `resize`, in two halves: the size asked for becomes an amount of the parent's
    /// extent, and the amount is taken from the siblings. Both halves are somewhere else —
    /// `percent_from_size_change` reads the request, `resize_child` moves the space — so this
    /// is only the sentence that joins them.
    fn resize_window_with_reach(
        &mut self,
        window: Option<&W::Id>,
        change: SizeChange,
        layout: Layout,
        reach: ResizeReach,
    ) {
        let Some(key) = self.window_target(window) else {
            return;
        };
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
        let target_span = self.tree.node_span(key, layout).unwrap_or(0.0);
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
                self.tree
                    .child_rect_in(parent_key, idx)
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
        if self
            .tree
            .resize_child(parent_key, child_idx, layout, reach, delta, space)
        {
            if parent_key == self.tree.workspace_root() {
                self.tree.layout();
            } else {
                self.tree.layout_container_subtree(parent_key);
            }
        }
    }

    pub fn set_fullscreen(&mut self, window: &W::Id, is_fullscreen: bool) -> bool {
        let selected_container = self.tree.selected_container_key();

        if is_fullscreen {
            if self.tree.window_owns_fullscreen(window) {
                return false;
            }

            let Some(key) = self.tiled_window_key(window) else {
                return false;
            };

            let already_focused = self
                .tree
                .focused_window()
                .is_some_and(|focused| focused.id() == window);
            if selected_container.is_none()
                && !already_focused
                && !self.tree.focus_window_by_id(window)
            {
                return false;
            }

            if let Some(tile) = self.tree.get_tile_mut(key) {
                tile.pending_maximized |= tile.window().pending_sizing_mode().is_maximized();
                tile.request_fullscreen(!self.options.animations.off, None);
            }

            self.tree.set_fullscreen_key(Some(key));
            self.tree.layout();
            if let Some(selected_key) = selected_container {
                self.tree.select_container(selected_key);
            }
            true
        } else {
            let Some(key) = self.tiled_window_key(window) else {
                return false;
            };
            let fullscreen_matches = self.tree.window_owns_fullscreen(window);
            let Some(tile) = self.tree.get_tile_mut(key) else {
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
            if tile.pending_maximized {
                tile.request_maximized(self.working_area.size, !self.options.animations.off, None);
            } else {
                tile.request_tile_size(self.working_area.size, !self.options.animations.off, None);
            }

            if fullscreen_matches {
                self.tree.set_fullscreen_key(None);
            }
            self.tree.layout();
            if let Some(selected_key) = selected_container {
                self.tree.select_container(selected_key);
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
        if self.tree.container_info(key).is_none() {
            return false;
        }
        let is_fullscreen = self.tree.fullscreen_key() == Some(key);

        if is_fullscreen {
            self.tree.set_fullscreen_key(None);
        } else {
            self.tree.set_fullscreen_key(Some(key));
        }

        self.tree.layout();
        true
    }

    fn sync_fullscreen_window(&mut self) {
        // A detached subtree carries client sizing state, but the destination workspace keeps
        // its own fullscreen pointer. Adopt the arriving fullscreen only when the destination
        // has none; otherwise revoke the stale request. This makes the single pointer true at
        // the protocol boundary too, rather than leaving two clients pending fullscreen.
        let tiled_root = self.tree.workspace_root();
        let pending_keys: Vec<_> = self
            .tree
            .tiles_in_branch(tiled_root)
            .into_iter()
            .filter(|tile| tile.window().pending_sizing_mode().is_fullscreen())
            .map(Tile::node_key)
            .collect();
        let authority = self.tree.fullscreen_key();
        let keep = authority.or_else(|| pending_keys.first().copied());
        if authority.is_none() {
            if let Some(key) = keep {
                self.tree.set_fullscreen_key(Some(key));
            }
        }

        for key in pending_keys.into_iter().filter(|key| Some(*key) != keep) {
            let Some(tile) = self.tree.get_tile_mut(key) else {
                continue;
            };
            if tile.pending_maximized {
                tile.request_maximized(self.working_area.size, !self.options.animations.off, None);
            } else {
                tile.request_tile_size(self.working_area.size, !self.options.animations.off, None);
            }
        }
    }

    pub fn set_maximized(&mut self, window: &W::Id, maximize: bool) -> bool {
        let Some(key) = self.tiled_window_key(window) else {
            return false;
        };
        let Some(tile) = self.tree.get_tile_mut(key) else {
            return false;
        };

        tile.pending_maximized = maximize;
        self.tree.layout();
        true
    }

    // No-ops: centering only makes sense with a scrolling viewport; in the i3 model the
    // viewport is fixed and every root child is always fully visible.
    pub fn center_column(&mut self) {}
    pub fn center_window(&mut self, _window: Option<&W::Id>) {}
    pub fn center_visible_columns(&mut self) {}

    pub fn expand_column_to_available_width(&mut self) {
        let Some(idx) = self.tree.focused_root_index() else {
            return;
        };
        let Some(root_key) = self.tree.root_node_key() else {
            return;
        };
        if self
            .tree
            .set_child_percent(root_key, idx, Layout::SplitH, 1.0)
        {
            self.tree.layout();
        }
    }

    /// sway's `swap container with`: exchange the selected node and the window `target`.
    pub fn swap_selected_with_window(&mut self, target: &W::Id) -> bool {
        self.mutate_tree(|tree| tree.swap_selected_with_window(target))
    }

    pub fn start_open_animation(&mut self, id: &W::Id) -> bool {
        let Some(key) = self.tiled_window_key(id) else {
            return false;
        };
        if let Some(tile) = self.tree.get_tile_mut(key) {
            tile.start_open_animation();
            return true;
        }
        false
    }
    pub fn start_close_animation_for_window<R: NiriRenderer>(
        &mut self,
        renderer: &mut R,
        window: &W::Id,
        blocker: crate::utils::transaction::TransactionBlocker,
    ) {
        let Some(key) = self.tiled_window_key(window) else {
            return;
        };

        let Some((rect, visible)) = self
            .tree
            .leaf_layouts()
            .iter()
            .find(|info| info.key == key)
            .map(|info| (info.rect, info.visible))
        else {
            return;
        };

        let visible = visible
            && self
                .render_fullscreen_container()
                .is_none_or(|scope| self.tree.is_descendant(key, scope));
        if !visible {
            return;
        }

        let Some(tile) = self.tree.get_tile_mut(key) else {
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
            self.options.animations.window_close.anim,
        );

        let scale = Scale::from(self.scale);
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
            Ok(closing) => {
                self.closing_windows.push(closing);
            }
            Err(err) => {
                warn!("error creating a closing window animation: {err:?}");
            }
        }
    }

    pub fn refresh(&mut self, is_active: bool, is_focused: bool) {
        let applied = self.tree.apply_pending_layouts_if_ready();
        if applied && self.tree.take_pending_relayout() {
            self.tree.layout();
        }
        let has_pending = self.tree.has_pending_layouts();
        let mut layouts = if has_pending {
            self.tree
                .pending_leaf_layouts_cloned()
                .unwrap_or_else(|| self.tree.leaf_layouts_cloned())
        } else {
            self.tree.leaf_layouts_cloned()
        };
        let tiled_root = self.tree.workspace_root();
        layouts.retain(|info| info.branch == tiled_root);
        let focused_key = self.focused_key();
        let fullscreen_container = self.render_fullscreen_container();
        let fullscreen_id = self.pending_fullscreen_window().cloned();
        let windowed_fullscreen_id = if fullscreen_id.is_none() && fullscreen_container.is_none() {
            self.focused_tile().and_then(|tile| {
                tile.window()
                    .is_pending_windowed_fullscreen()
                    .then(|| tile.window().id().clone())
            })
        } else {
            None
        };

        let ctx = WindowStateContext {
            focused_key,
            workspace_active: is_active,
            deactivate_unfocused: self.options.deactivate_unfocused_windows && !is_focused,
            request_size: !has_pending,
            working_area_size: self.working_area.size,
            options: &self.options,
            fullscreen_id: fullscreen_id.as_ref(),
            windowed_fullscreen_id: windowed_fullscreen_id.as_ref(),
            has_fullscreen_container: fullscreen_container.is_some(),
            view_size: self.view_size,
        };
        // See the other state pass: never drive window state from a stale snapshot.
        let skip_state_pass = self.tree.pending_layout_is_stale();
        let resize_data = self.interactive_resize_data_by_leaf(&layouts);
        for info in layouts {
            if skip_state_pass {
                break;
            }
            let in_fullscreen_container =
                fullscreen_container.is_some_and(|scope| self.tree.is_descendant(info.key, scope));
            // Use O(1) key lookup instead of O(depth) path lookup.
            if let Some(tile) = self.tree.get_tile_mut(info.key) {
                let resize = resize_data.get(&info.key).copied();
                Self::update_window_state(tile, &info, resize, in_fullscreen_container, &ctx);
            }
        }
    }
    pub fn render_above_top_layer(&self) -> bool {
        // Render above the top layer (e.g. waybar) for either fullscreen authority.
        self.render_fullscreen_container().is_some()
            || self.render_fullscreen_window().is_some()
            || self
                .focused_tile()
                .is_some_and(|tile| tile.window().is_pending_windowed_fullscreen())
    }

    pub fn activation_view_distance(&self, _window: &W::Id) -> f64 {
        self.viewport.activation_distance()
    }

    pub fn popup_target_rect(&self, window: &W::Id) -> Option<Rectangle<f64, Logical>> {
        // Find the tile for this window and return its popup target rectangle
        for info in self.display_layouts() {
            if let Some(tile) = self.tree.get_tile(info.key) {
                if tile.window().id() == window {
                    // Similar to tiling layout: constrain horizontally to window,
                    // vertically to the working area
                    let width = tile.window_size().w;
                    let height = self.working_area.size.h;

                    let mut target = Rectangle::from_size(Size::from((width, height)));
                    target.loc.y += self.working_area.loc.y;
                    target.loc.y -= info.rect.loc.y;
                    target.loc.y -= tile.window_loc().y;

                    return Some(target);
                }
            }
        }
        None
    }

    pub fn horizontal_view_gesture_begin(&mut self, is_touchpad: bool) {
        self.viewport.begin_horizontal_gesture(is_touchpad);
    }

    pub fn horizontal_view_gesture_update(
        &mut self,
        delta: f64,
        timestamp: Duration,
        is_touchpad: bool,
    ) -> Option<bool> {
        self.viewport
            .update_horizontal_gesture(delta, timestamp, is_touchpad)
    }

    pub fn horizontal_view_gesture_end(&mut self, cancelled: Option<bool>) -> bool {
        self.viewport.end_horizontal_gesture(cancelled)
    }
}

impl<W: LayoutElement> TreeSpace<W> {
    pub(crate) fn layout_tree(&self) -> Option<LayoutTreeNode> {
        self.apply_workspace_fullscreen(self.tree.layout_tree()?)
    }

    pub(crate) fn layout_tree_unfocused(&self) -> Option<LayoutTreeNode> {
        self.apply_workspace_fullscreen(self.tree.layout_tree_unfocused()?)
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
        mut root: LayoutTreeNode,
    ) -> Option<LayoutTreeNode> {
        let Some(fullscreen_key) = self.tree.fullscreen_key() else {
            return Some(root);
        };
        let fullscreen_windows: HashSet<u64> = self
            .tree
            .tiles_in_branch(fullscreen_key)
            .into_iter()
            .map(|tile| tile.window().ipc_id())
            .collect();
        let fullscreen_leaf = self
            .tree
            .get_tile(fullscreen_key)
            .map(|tile| tile.window().ipc_id());
        let fullscreen_leaf_uses_output = self
            .tree
            .leaf_layouts()
            .iter()
            .find(|info| info.key == fullscreen_key)
            .is_some_and(|info| info.workspace_fullscreen);
        let view = LayoutTreeRect {
            x: 0.0,
            y: 0.0,
            width: self.view_size.w,
            height: self.view_size.h,
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

impl<W: LayoutElement> TreeSpace<W> {
    fn update_window_state(
        tile: &mut Tile<W>,
        info: &LeafLayoutInfo,
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

impl<W: LayoutElement> RootTilingSubtree<W> {
    /// A tiled container moved to another workspace gets a new share there while every sizing
    /// value below it remains attached to its descendant node.
    ///
    /// sway/commands/move.c:198-239
    pub(super) fn prepare_for_workspace_move(&mut self) {
        self.subtree.unset_root_fractions();
    }

    pub fn new(tile: Tile<W>) -> Self {
        Self {
            subtree: DetachedNode::Leaf(tile),
        }
    }

    pub fn from_tiles(tiles: Vec<Tile<W>>) -> Self {
        if tiles.is_empty() {
            return Self {
                subtree: DetachedNode::Container(DetachedContainer::new(
                    Layout::SplitV,
                    Vec::new(),
                )),
            };
        }

        if tiles.len() == 1 {
            return Self::new(tiles.into_iter().next().unwrap());
        }

        let children = tiles
            .into_iter()
            .map(DetachedNode::Leaf)
            .collect::<Vec<_>>();
        Self {
            subtree: DetachedNode::Container(DetachedContainer::new(Layout::SplitV, children)),
        }
    }

    pub fn tiles(&self) -> Vec<&Tile<W>> {
        self.subtree.tiles()
    }

    pub fn contains(&self, window: &W) -> bool {
        self.subtree.contains_window(window.id())
    }

    pub fn from_subtree(subtree: DetachedNode<W>) -> Self {
        Self { subtree }
    }

    pub fn into_subtree(self) -> DetachedNode<W> {
        self.subtree
    }

    pub fn into_tiles(self) -> Vec<Tile<W>> {
        self.subtree.into_tiles()
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
    tree: &ContainerTree<W>,
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
/// branch. Each cached layout carries the branch it was arranged in, so this is a filter
/// rather than a walk up the tree per leaf.
pub(super) fn branch_display_layouts<'a, W: LayoutElement>(
    tree: &'a ContainerTree<W>,
    branch: NodeKey,
) -> impl DoubleEndedIterator<Item = &'a LeafLayoutInfo> + 'a {
    // The committed layouts are what is on screen, so they are the answer whenever this
    // branch has any. It has none only before its first arrange has landed, and then the
    // pending ones are the only answer there is — asking a neighbour's would be worse than
    // saying nothing.
    let committed = tree.leaf_layouts();
    let source = if committed.iter().any(|info| info.branch == branch) {
        committed
    } else {
        tree.pending_leaf_layouts_for(branch).unwrap_or(committed)
    };
    source.iter().filter(move |info| info.branch == branch)
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
/// [`TreeSpace::ppt_reference`].
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

/// What a tile needs to know about the space it is in: see [`TreeSpace::tile_config`].
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
