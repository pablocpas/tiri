use std::cmp::max;

use log::warn;
use smithay::backend::renderer::gles::GlesRenderer;
use smithay::utils::{Logical, Point, Rectangle, Scale, Serial, Size};
use tiri_config::utils::MergeWith as _;
use tiri_config::{PresetSize, RelativeTo};
use tiri_ipc::{PositionChange, SizeChange};

use super::closing_window::{ClosingWindow, ClosingWindowRenderElement};
use super::container::{
    floating_position_from_logical, scale_floating_position, Direction, FloatingRootKind,
    InactiveTilingReference, IncomingView, InsertParentInfo, LayoutMode, NodeKey, TabBarInfo,
};
use super::container_tree::{percent_from_size_change, ContainerTree, LeafFrameInfo, TileConfig};
use super::focus_ring::{render_container_selection, FocusRingEdges, FocusRingRenderElement};
use super::tile::{Tile, TileRenderElement, TileRenderSnapshot};
use super::workspace::{InteractiveResize, ResolvedSize};
use super::{
    resize_edges_for_point, BranchRenderParams, ConfigureIntent, InteractiveResizeData,
    LayoutElement, ResizeAxis, ResizeRequest, SizeFrac, ViewTransfer,
};
use crate::animation::Animation;
use crate::layout::RenderLayer;
use crate::niri_render_elements;
use crate::render_helpers::primary_gpu_texture::PrimaryGpuTextureRenderElement;
use crate::render_helpers::renderer::NiriRenderer;
use crate::render_helpers::xray::XrayPos;
use crate::render_helpers::RenderCtx;
use crate::utils::transaction::TransactionBlocker;
use crate::utils::{
    center_preferring_top_left_in_area, clamp_preferring_top_left_in_area,
    ensure_min_max_size_maybe_zero, ResizeEdge,
};
use crate::window::ResolvedWindowRules;

/// By how many logical pixels the directional move commands move floating windows.
///
/// sway's `cmd_move_in_direction`: `move_amt` starts at 10 and a script overrides it by
/// naming a distance. Ten is the default a `move left` with no argument gets.
pub const DIRECTIONAL_MOVE_PX: f64 = 10.;

/// Floating-layer behavior bound to one workspace in the shared tree.
///
/// This is not a second space or tree. Root membership, topology, geometry, fullscreen and
/// focus all live in [`ContainerTree`], beside the tiled roots of the same workspace. The
/// layer keeps only state that is inherently specific to its interaction/render pass: an
/// active floating resize, closing animations and reusable render scratch.
///
/// Positional commands remain a separate behavior namespace because they are genuinely
/// different operations: tiled `move_up` reparents a node, while floating `move_up` translates
/// a root. Passing the shared tree is dependency access, never ownership transfer.
#[derive(Debug)]
pub(super) struct FloatingRuntime<W: LayoutElement> {
    /// Workspace whose floating roots this projection owns in the shared tree.
    ///
    /// The workspace beside it holds the same key, and that is a handle rather than a second
    /// authority: it is written once at construction and never reassigned, on either side.
    /// Threading it through instead would put it back into the hundred-odd signatures the
    /// tree and the container tree just stopped carrying, to remove a value that cannot
    /// disagree with itself. `verify_invariants` asserts the pairing so that stays true.
    workspace: NodeKey,
    /// Ongoing interactive resize.
    interactive_resize: Option<InteractiveResize<W>>,

    /// Windows in the closing animation.
    closing_windows: Vec<ClosingWindow>,

    /// Copy-only projection reused while rendering floating branches.
    render_layout_scratch: Vec<LeafFrameInfo>,
}

niri_render_elements! {
    FloatingRenderElement<R> => {
        Tile = TileRenderElement<R>,
        TabBar = PrimaryGpuTextureRenderElement,
        ClosingWindow = ClosingWindowRenderElement,
        ContainerSelection = FocusRingRenderElement,
    }
}

/// Which of a workspace's floating roots a render pass draws.
///
/// The output draws its sticky floats apart from the rest of the active workspace while that
/// workspace slides in, so they stay where they are; otherwise every root is drawn together,
/// in stacking order.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum FloatingRoots {
    All,
    Sticky,
    NonSticky,
}

impl FloatingRoots {
    fn includes<W: LayoutElement>(self, containers: &ContainerTree<W>, root: NodeKey) -> bool {
        match self {
            Self::All => true,
            Self::Sticky => containers.is_sticky(root),
            Self::NonSticky => !containers.is_sticky(root),
        }
    }

    fn includes_node<W: LayoutElement>(self, containers: &ContainerTree<W>, key: NodeKey) -> bool {
        self == Self::All || self.includes(containers, containers.branch_root(key))
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum FloatingResizeAnchor {
    Center,
    KeepOrigin,
}

#[derive(Debug, Clone, Copy)]
pub(super) struct FloatingResizeHit<WId> {
    pub window: WId,
    pub edges: ResizeEdge,
    pub external_edges: ResizeEdge,
}

#[derive(Debug, Clone, Copy)]
pub(super) enum FloatingResizeResult<WId> {
    None,
    Blocked,
    Hit(FloatingResizeHit<WId>),
}

impl<W: LayoutElement> FloatingRuntime<W> {
    fn leaf_point_hits_tab_bar(
        tree: &super::container::ContainerTree<W>,
        tab_bar_infos: &[TabBarInfo],
        branch: NodeKey,
        leaf_key: NodeKey,
        pos_in_container: Point<f64, Logical>,
        gap: f64,
        scale: f64,
    ) -> bool {
        let eps = 1.0 / scale.max(1e-6);
        tab_bar_infos.iter().any(|info| {
            if !tree.is_descendant(leaf_key, info.key) {
                return false;
            }

            let mut rect = info.rect;
            if gap > 0.0 && info.key == branch {
                rect.loc.x -= gap;
                rect.loc.y -= gap;
                rect.size.w = (rect.size.w + gap * 2.0).max(0.0);
            }

            rect.loc.x -= eps;
            rect.loc.y -= eps;
            rect.size.w += eps * 2.0;
            rect.size.h += eps * 2.0;

            rect.contains(pos_in_container)
        })
    }

    fn leaf_has_tab_bar_ancestor(
        tree: &super::container::ContainerTree<W>,
        tab_bar_infos: &[TabBarInfo],
        leaf_key: NodeKey,
    ) -> bool {
        tab_bar_infos
            .iter()
            .any(|info| tree.is_descendant(leaf_key, info.key))
    }

    fn external_edges_for_rect(
        container_size: Size<f64, Logical>,
        rect: Rectangle<f64, Logical>,
        edges: ResizeEdge,
    ) -> ResizeEdge {
        const EDGE_EPSILON: f64 = 0.5;

        let mut external = ResizeEdge::empty();
        if (rect.loc.x - 0.0).abs() <= EDGE_EPSILON {
            external |= ResizeEdge::LEFT;
        }
        if (rect.loc.x + rect.size.w - container_size.w).abs() <= EDGE_EPSILON {
            external |= ResizeEdge::RIGHT;
        }
        if (rect.loc.y - 0.0).abs() <= EDGE_EPSILON {
            external |= ResizeEdge::TOP;
        }
        if (rect.loc.y + rect.size.h - container_size.h).abs() <= EDGE_EPSILON {
            external |= ResizeEdge::BOTTOM;
        }

        external & edges
    }

    pub fn new(workspace: NodeKey) -> Self {
        Self {
            workspace,
            interactive_resize: None,
            closing_windows: Vec::new(),
            render_layout_scratch: Vec::new(),
        }
    }

    fn root(&self, containers: &ContainerTree<W>, idx: usize) -> NodeKey {
        containers
            .floating_roots_in(self.workspace)
            .get(idx)
            .copied()
            .expect("floating index must name an authoritative root entry")
    }

    fn root_count(&self, containers: &ContainerTree<W>) -> usize {
        containers.floating_roots_in(self.workspace).len()
    }

    fn container_area(&self, containers: &ContainerTree<W>, idx: usize) -> Rectangle<f64, Logical> {
        let root = self.root(containers, idx);
        containers
            .floating_container_area(root)
            .expect("every floating stack entry must name a floating root")
    }

    fn set_container_logical_pos(
        &mut self,
        containers: &mut ContainerTree<W>,
        idx: usize,
        logical_pos: Point<f64, Logical>,
    ) -> Point<f64, Logical> {
        let root = self.root(containers, idx);
        containers.set_floating_logical_pos(root, logical_pos)
    }

    fn set_container_size(
        &mut self,
        containers: &mut ContainerTree<W>,
        idx: usize,
        size: Size<f64, Logical>,
    ) -> Rectangle<f64, Logical> {
        let root = self.root(containers, idx);
        containers.set_floating_size(root, size)
    }

    /// The workspace this floating side belongs to.
    pub(super) fn workspace(&self) -> NodeKey {
        self.workspace
    }

    pub(super) fn advance_closing_animations(&mut self) {
        self.closing_windows.retain_mut(|closing| {
            closing.advance_animations();
            closing.are_animations_ongoing()
        });
    }

    pub(super) fn has_closing_windows(&self) -> bool {
        !self.closing_windows.is_empty()
    }

    pub fn update_render_elements(
        &mut self,
        containers: &mut ContainerTree<W>,
        is_active: bool,
        view_rect: Rectangle<f64, Logical>,
        layer: RenderLayer,
    ) {
        let _span = tracy_client::span!("FloatingRuntime::update_render_elements");
        let active = containers.active_floating_window_id(self.workspace);
        let fullscreen_id = containers
            .floating_fullscreen_window_id(self.workspace)
            .cloned();
        let selection_is_container = self
            .active_container_idx(containers)
            .is_some_and(|idx| self.selected_is_container_in(containers, idx));
        let scale = containers.scale(self.workspace);
        let floating_has_focus = is_active;
        let workspace = self.workspace;
        containers.apply_pending_layouts_and_deferred_relayout(workspace);
        let mut layouts = std::mem::take(&mut self.render_layout_scratch);
        let roots: Vec<_> = containers.floating_roots(self.workspace).collect();
        let active_children = containers.active_children();
        for root in roots {
            layouts.clear();
            {
                let _span =
                    tracy_client::span!("FloatingRuntime::project_display_layouts_for_render");
                layouts.extend(
                    super::container_tree::branch_display_layouts(containers, root)
                        .map(|info| LeafFrameInfo::new(root, info)),
                );
            }
            #[cfg(feature = "profile-with-tracy")]
            {
                tracy_client::plot!("layout.floating_leaf_projections", layouts.len() as f64);
            }
            // sway's `render_floating_container`: a float holding a single view is `focused`,
            // `urgent` or `unfocused`, and nothing else — the focus-inactive comparison never
            // runs on it, because that view *is* the floating con and floating cons are not
            // compared against each other. Only when the float holds a real sub-layout does
            // sway recurse into `render_container`, and then the per-level rule applies to the
            // windows inside it as it would anywhere else.
            let float_has_sublayout = containers.window_count_in_branch(root) > 1;
            for info in layouts.iter().copied() {
                let is_focus_head =
                    float_has_sublayout && active_children.is_focus_head(containers, info.key);
                // Same rule as tiling: under a tab bar the tab is the top decoration.
                let mut edges = FocusRingEdges::all();
                if containers.parent_is_switcher(info.key) {
                    edges.top = false;
                }
                if let Some(tile) = containers.get_tile_mut(info.key) {
                    // Skip tiles belonging to a different render layer.
                    if layer.is_normal() == tile.is_moving_between_workspaces() {
                        continue;
                    }

                    let is_fullscreen_tile = fullscreen_id
                        .as_ref()
                        .is_some_and(|id| id == tile.window().id());

                    let tile_view_rect = if is_fullscreen_tile {
                        view_rect
                    } else {
                        let mut pos = info.rect.loc + tile.render_offset();
                        pos = pos.to_physical_precise_round(scale).to_logical(scale);
                        let mut r = view_rect;
                        r.loc -= pos;
                        r
                    };

                    // `is_active` is the workspace, not the focus: the floating side owns
                    // the focus only when the workspace says it does. Reading it as focus
                    // painted this workspace's active window `focused` while a tiled window
                    // held the keyboard, so both wore the focused border at once.
                    let is_focused = if is_fullscreen_tile {
                        floating_has_focus
                    } else {
                        floating_has_focus
                            && Some(tile.window().id()) == active.as_ref()
                            && !selection_is_container
                    };
                    tile.update_render_elements(
                        is_active,
                        is_focused,
                        is_focus_head,
                        edges,
                        None,
                        tile_view_rect,
                    );
                }
            }
        }
        self.render_layout_scratch = layouts;
    }

    pub(super) fn resize_hit_under(
        &self,
        containers: &ContainerTree<W>,
        pos: Point<f64, Logical>,
    ) -> FloatingResizeResult<W::Id> {
        let tree = containers;
        if containers.floating_fullscreen_key(self.workspace).is_some() {
            return FloatingResizeResult::None;
        }

        let scale = Scale::from(containers.scale(self.workspace));
        for root in tree.floating_roots(self.workspace) {
            let container_area = tree
                .floating_container_area(root)
                .expect("every floating stack entry must name a floating root");
            let gap = containers.branch_gap(root);
            let tab_bar_infos = tree.tab_bar_layouts_in_branch(root);
            for info in super::container_tree::branch_display_layouts(containers, root)
                .filter(|info| info.visible)
            {
                let Some(tile) = tree.get_tile(info.key) else {
                    continue;
                };

                let mut tile_pos = info.rect.loc + tile.render_offset();
                tile_pos = tile_pos.to_physical_precise_round(scale).to_logical(scale);
                let tile_rect = Rectangle::new(tile_pos, info.rect.size);
                let border = tile.effective_border_width().unwrap_or(0.0) * 2.0;
                let threshold = super::RESIZE_EDGE_THRESHOLD.max(border);
                let expanded_rect = Rectangle::new(
                    Point::from((tile_rect.loc.x - threshold, tile_rect.loc.y - threshold)),
                    Size::from((
                        tile_rect.size.w + threshold * 2.0,
                        tile_rect.size.h + threshold * 2.0,
                    )),
                );

                if !expanded_rect.contains(pos) {
                    continue;
                }

                if Self::leaf_point_hits_tab_bar(
                    tree,
                    &tab_bar_infos,
                    root,
                    info.key,
                    pos,
                    gap,
                    containers.scale(self.workspace),
                ) {
                    return FloatingResizeResult::Blocked;
                }

                let pos_within_tile = pos - tile_pos;
                let size = tile.tile_size();
                let mut edges =
                    resize_edges_for_point(pos_within_tile, size, tile.effective_border_width());
                if Self::leaf_has_tab_bar_ancestor(tree, &tab_bar_infos, info.key) {
                    edges.remove(ResizeEdge::TOP);
                }
                if edges.is_empty() {
                    return FloatingResizeResult::Blocked;
                }

                let mut local_rect = info.rect;
                local_rect.loc -= container_area.loc;
                let external_edges =
                    Self::external_edges_for_rect(container_area.size, local_rect, edges);
                return FloatingResizeResult::Hit(FloatingResizeHit {
                    window: tile.window().id().clone(),
                    edges,
                    external_edges,
                });
            }
        }

        FloatingResizeResult::None
    }

    fn tiles_with_offsets_visible<'a>(
        &'a self,
        containers: &'a ContainerTree<W>,
    ) -> impl Iterator<Item = (&'a Tile<W>, Point<f64, Logical>)> + 'a {
        containers
            .floating_tiles_with_offsets(self.workspace, true)
            .map(|(_, tile, pos)| (tile, pos))
    }

    fn tab_bar_hit<'a>(
        &'a self,
        containers: &'a ContainerTree<W>,
        pos: Point<f64, Logical>,
    ) -> Option<(&'a W, super::HitType)> {
        // A 1px pad makes the floating bar's edges forgiving to hit: next to them is the
        // desktop, not another window.
        containers
            .floating_roots(self.workspace)
            .find_map(|root| containers.branch_tab_bar_hit(root, pos, 1))
    }

    pub fn window_under<'a>(
        &'a self,
        containers: &'a ContainerTree<W>,
        pos: Point<f64, Logical>,
    ) -> Option<(&'a W, super::HitType)> {
        if let Some(fullscreen_id) = containers.floating_fullscreen_window_id(self.workspace) {
            let tile = containers
                .floating_tiles_in_workspace(self.workspace)
                .find(|t| t.window().id() == fullscreen_id)?;
            return super::HitType::hit_tile(tile, Point::from((0.0, 0.0)), pos);
        }

        let fullscreen_scope = containers.floating_fullscreen_key(self.workspace);
        let tab_hit = if let Some(scope) = fullscreen_scope {
            containers.branch_tab_bar_hit(scope, pos, 1)
        } else {
            self.tab_bar_hit(containers, pos)
        };
        if let Some(hit) = tab_hit {
            return Some(hit);
        }

        for (key, tile, tile_pos) in
            containers.floating_tiles_with_render_positions_keyed(self.workspace)
        {
            if fullscreen_scope.is_some_and(|scope| !containers.is_descendant(key, scope)) {
                continue;
            }
            if let Some(rv) = super::HitType::hit_tile(tile, tile_pos, pos) {
                return Some(rv);
            }
        }

        None
    }

    fn idx_of(&self, containers: &ContainerTree<W>, id: &W::Id) -> Option<usize> {
        let tree = containers;
        let key = tree.window_key(id)?;
        let root = tree.branch_root(key);
        tree.floating_root_index(self.workspace, root)
    }

    fn contains(&self, containers: &ContainerTree<W>, id: &W::Id) -> bool {
        self.idx_of(containers, id).is_some()
    }

    fn active_container_idx(&self, containers: &ContainerTree<W>) -> Option<usize> {
        let active_id = containers.active_floating_window_id(self.workspace)?;
        self.idx_of(containers, &active_id)
    }

    fn selected_is_container_in(&self, containers: &ContainerTree<W>, idx: usize) -> bool {
        containers.selected_container_in(self.root(containers, idx))
    }

    /// The node a command targets inside container `idx`: its root when the whole floating
    /// wrapper is selected, otherwise the tree's own selection.
    fn selected_key_in(&self, containers: &ContainerTree<W>, idx: usize) -> Option<NodeKey> {
        let tree = containers;
        tree.branch_position(self.root(containers, idx))
    }

    fn tile_at_mut<'a>(
        &self,
        containers: &'a mut ContainerTree<W>,
        id: &W::Id,
    ) -> Option<&'a mut Tile<W>> {
        let tree = &mut *containers;
        let key = tree.window_key(id)?;
        (tree.workspace_of(key) == Some(self.workspace) && tree.is_in_floating_branch(key))
            .then(|| tree.get_tile_mut(key))
            .flatten()
    }

    pub fn set_fullscreen(
        &mut self,
        containers: &mut ContainerTree<W>,
        window: &W::Id,
        is_fullscreen: bool,
    ) {
        if is_fullscreen {
            if containers.floating_window_owns_fullscreen(self.workspace, window) {
                return;
            }

            if let Some(previous) = containers
                .floating_fullscreen_window_id(self.workspace)
                .cloned()
            {
                if previous != *window {
                    self.set_fullscreen(containers, &previous, false);
                }
            }

            let Some(key) = containers.window_key(window).filter(|key| {
                let tree = &*containers;
                tree.workspace_of(*key) == Some(self.workspace) && tree.is_in_floating_branch(*key)
            }) else {
                return;
            };

            // Store the floating size before going fullscreen.
            if let Some(tile) = containers
                .floating_tiles_mut_in_workspace(self.workspace)
                .find(|t| t.window().id() == window)
            {
                Self::store_floating_size_for_restore(tile);
                tile.request_fullscreen(true, None);
            }

            containers.set_fullscreen_key(self.workspace, Some(key));
        } else {
            if !containers.floating_window_owns_fullscreen(self.workspace, window) {
                return;
            }

            // A one-window floating root records client commits as its resize base without
            // retargeting an in-flight compositor size. Fullscreen is precisely such a target;
            // restore the live base now so the ordinary floating arrange below cannot overwrite
            // the saved client size with the pre-fullscreen target.
            if let Some(idx) = self.idx_of(containers, window) {
                let restore_size = self.container_area(containers, idx).size;
                self.set_container_size(containers, idx, restore_size);
            }

            // Restore the floating size.
            if let Some(tile) = containers
                .floating_tiles_mut_in_workspace(self.workspace)
                .find(|t| t.window().id() == window)
            {
                let size = tile.floating_window_size.unwrap_or_default();
                tile.window_mut().request_size_once(size, true);
            }

            containers.set_fullscreen_key(self.workspace, None);
        }
        let workspace = self.workspace;
        containers.layout(workspace);
    }

    pub(super) fn add_tile(
        &mut self,
        containers: &mut ContainerTree<W>,
        tile: IncomingView<W>,
        activate: bool,
    ) {
        self.add_tile_at(containers, 0, tile, activate);
    }

    pub(super) fn add_tile_with_restore_hint(
        &mut self,
        containers: &mut ContainerTree<W>,
        mut tile: IncomingView<W>,
        activate: bool,
    ) {
        let hint = containers
            .incoming_tile_mut(&mut tile)
            .expect("a floating insertion must own a valid incoming view")
            .floating_reinsert_hint
            .take();

        // A tile that was the whole group has no position inside one to be restored to, and
        // the group it was went away with it; it comes back the way it left, as its own. Nor
        // can a remembered position be restored into a group that is now one window: there is
        // no container there to hold two.
        if let Some((container_id, Some(insert_info))) = hint {
            if let Some(idx) = containers
                .floating_root_index_by_id(self.workspace, container_id)
                .filter(|idx| !containers.branch_root_is_lone_window(self.root(containers, *idx)))
            {
                self.add_tile_to_container_idx_with_parent_info(
                    containers,
                    idx,
                    tile,
                    activate,
                    &insert_info,
                );
                return;
            }
        }

        self.add_tile(containers, tile, activate);
    }

    fn prepare_tile_for_floating(
        config: &TileConfig,
        tile: &mut Tile<W>,
    ) -> (W::Id, Option<Size<f64, Logical>>) {
        tile.update_config(config.view_size, config.scale, config.options.clone());

        let win_id = tile.window().id().clone();

        // Restore the previous floating window size, and in case the tile is fullscreen,
        // unfullscreen it.
        let animate = !tile.is_scratchpad();
        let mut requested_window_size = None;
        {
            let floating_size = tile.floating_window_size;
            let win = tile.window_mut();
            let mut size = if !win.pending_sizing_mode().is_normal() {
                // If the window was fullscreen or maximized without a floating size, ask for (0,
                // 0).
                floating_size.unwrap_or_default()
            } else {
                // If the window wasn't fullscreen without a floating size (e.g. it was tiled
                // before), ask for the current size. If the current size is unknown
                // (the window was only ever fullscreen until now), fall back to (0,
                // 0).
                floating_size.unwrap_or_else(|| win.expected_size().unwrap_or_default())
            };

            // Apply min/max size window rules. If requesting a concrete size, apply completely; if
            // requesting (0, 0), apply only when min/max results in a fixed size.
            let min_size = win.min_size();
            let max_size = win.max_size();
            size.w = ensure_min_max_size_maybe_zero(size.w, min_size.w, max_size.w);
            size.h = ensure_min_max_size_maybe_zero(size.h, min_size.h, max_size.h);

            if size.w > 0 && size.h > 0 {
                requested_window_size = Some(size);
            }
            win.request_size_once(size, animate);
        }

        let requested_tile_size = requested_window_size.map(|size| {
            Size::from((
                tile.tile_width_for_window_width(f64::from(size.w)),
                tile.tile_height_for_window_height(f64::from(size.h)),
            ))
        });

        (win_id, requested_tile_size)
    }

    /// sway's `floating_calculate_constraints` on its automatic settings.
    ///
    /// A floor of 75 by 50 with the output layout box as the ceiling, the floor applied outside
    /// the ceiling so it wins on an output too small to hold it, then the client's own limits.
    fn floating_constraints(
        config: &TileConfig,
        tile: &Tile<W>,
        mut size: Size<i32, Logical>,
    ) -> Size<i32, Logical> {
        size.w = size.w.min(config.view_size.w.floor() as i32).max(75);
        size.h = size.h.min(config.view_size.h.floor() as i32).max(50);
        let min_size = tile.window().min_size();
        let max_size = tile.window().max_size();
        size.w = ensure_min_max_size_maybe_zero(size.w, min_size.w, max_size.w);
        size.h = ensure_min_max_size_maybe_zero(size.h, min_size.h, max_size.h);
        size
    }

    fn preserve_fullscreen_tile_for_floating(
        config: &TileConfig,
        tile: &mut Tile<W>,
        restore_tile_size: Option<Size<f64, Logical>>,
    ) {
        tile.update_config(config.view_size, config.scale, config.options.clone());
        if tile.floating_window_size.is_none() {
            tile.floating_window_size = restore_tile_size
                .filter(|size| size.w > 0.0 && size.h > 0.0)
                .map(|size| tile.requested_window_size_for_tile(size, tile.tab_bar_offset()))
                .or_else(|| {
                    let size = tile.window().natural_size();
                    (size.w > 0 && size.h > 0).then_some(size)
                })
                // The constraints apply to any view that becomes floating, fullscreen or not.
                // Without them a window that mapped smaller than sway's floor restores to that
                // size instead of to 75x50.
                .map(|size| Self::floating_constraints(config, tile, size));
        }
    }

    fn add_tile_at(
        &mut self,
        containers: &mut ContainerTree<W>,
        mut idx: usize,
        mut tile: IncomingView<W>,
        activate: bool,
    ) {
        let config = containers.tile_config(self.workspace);
        // A view that maps fullscreen keeps the state on the list it lands in —
        // `container_set_fullscreen` never moves a node between lists, so arriving floating is
        // not a reason to leave fullscreen. Only a view that is not fullscreen gets normalized
        // to a floating size on the way in.
        let maps_fullscreen = containers
            .incoming_tile(&tile)
            .expect("a floating insertion must own a valid incoming view")
            .window()
            .pending_sizing_mode()
            .is_fullscreen();
        let requested_tile_size = if maps_fullscreen {
            Self::preserve_fullscreen_tile_for_floating(
                &config,
                containers
                    .incoming_tile_mut(&mut tile)
                    .expect("a floating insertion must own a valid incoming view"),
                None,
            );
            // The group's box is the one the view would have had unfullscreened.
            // `container_init_floating` computes it from the natural size whatever the
            // fullscreen state, and it is what `fullscreen disable` restores to later.
            containers
                .incoming_tile(&tile)
                .expect("a floating insertion must own a valid incoming view")
                .floating_window_size
                .map(|size| {
                    let tile = containers
                        .incoming_tile(&tile)
                        .expect("a floating insertion must own a valid incoming view");
                    Size::from((
                        tile.tile_width_for_window_width(f64::from(size.w)),
                        tile.tile_height_for_window_height(f64::from(size.h)),
                    ))
                })
        } else {
            Self::prepare_tile_for_floating(
                &config,
                containers
                    .incoming_tile_mut(&mut tile)
                    .expect("a floating insertion must own a valid incoming view"),
            )
            .1
        };

        // Make sure the tile isn't inserted below its parent.
        let roots: Vec<_> = containers
            .floating_roots(self.workspace)
            .take(idx)
            .collect();
        for (i, root) in roots.into_iter().enumerate() {
            if containers.windows_in_branch(root).iter().any(|parent| {
                containers
                    .incoming_tile(&tile)
                    .expect("a floating insertion must own a valid incoming view")
                    .window()
                    .is_child_of(parent)
            }) {
                idx = i;
                break;
            }
        }

        let tile_ref = containers
            .incoming_tile(&tile)
            .expect("a floating insertion must own a valid incoming view");
        let tile_size = requested_tile_size.unwrap_or_else(|| tile_ref.tile_size());
        let pos = self
            .stored_or_default_tile_pos(containers.working_area(self.workspace), tile_ref)
            .unwrap_or_else(|| {
                center_preferring_top_left_in_area(
                    containers.working_area(self.workspace),
                    tile_size,
                )
            });
        let rect = Rectangle::new(pos, tile_size);

        let workspace = self.workspace;
        let (_root, leaf) = containers.float_new_group(workspace, tile, rect, idx);
        // `workspace->fullscreen` names the node on whichever list it lives.
        if maps_fullscreen && containers.fullscreen_key(self.workspace).is_none() {
            containers.set_fullscreen_key(self.workspace, Some(leaf));
        }
        if activate || containers.focused_node_key().is_none() {
            containers.focus_node(leaf);
        }
        let workspace = self.workspace;
        containers.layout(workspace);

        self.bring_up_descendants_of(containers, idx);
    }

    pub(super) fn add_tile_to_active_container(
        &mut self,
        containers: &mut ContainerTree<W>,
        tile: IncomingView<W>,
        activate: bool,
    ) -> bool {
        let Some(idx) = self.active_container_idx(containers) else {
            return false;
        };
        self.add_tile_to_container_idx(containers, idx, tile, activate)
    }

    pub(super) fn add_tile_to_container_of(
        &mut self,
        containers: &mut ContainerTree<W>,
        id: &W::Id,
        tile: IncomingView<W>,
        activate: bool,
    ) -> bool {
        let Some(idx) = self.idx_of(containers, id) else {
            return false;
        };

        self.add_tile_to_container_idx(containers, idx, tile, activate)
    }

    fn add_tile_to_container_idx(
        &mut self,
        containers: &mut ContainerTree<W>,
        idx: usize,
        mut tile: IncomingView<W>,
        activate: bool,
    ) -> bool {
        let config = containers.tile_config(self.workspace);
        let (win_id, _) = Self::prepare_tile_for_floating(
            &config,
            containers
                .incoming_tile_mut(&mut tile)
                .expect("a floating insertion must own a valid incoming view"),
        );
        let root = self.root(containers, idx);
        if containers.selected_container_key() == Some(root) {
            let insert_idx = containers.branch_children_len(root);
            containers.insert_leaf_into_branch(root, insert_idx, tile, activate);
        } else {
            containers.insert_window_into_branch(root, tile, activate);
        }
        // Sway's view-map path arranges the new view's parent, not the workspace. That is
        // observable when this group contains workspace fullscreen: a workspace arrange
        // stops at the fullscreen owner, while `arrange_container(root)` still lays out the
        // complete floating group in its ordinary box.
        containers.layout_container_subtree(root);

        if activate {
            self.activate_window(containers, &win_id);
        }

        true
    }

    fn add_tile_to_container_idx_with_parent_info(
        &mut self,
        containers: &mut ContainerTree<W>,
        idx: usize,
        mut tile: IncomingView<W>,
        activate: bool,
        info: &InsertParentInfo,
    ) {
        let config = containers.tile_config(self.workspace);
        let (win_id, _) = Self::prepare_tile_for_floating(
            &config,
            containers
                .incoming_tile_mut(&mut tile)
                .expect("a floating insertion must own a valid incoming view"),
        );

        let root = self.root(containers, idx);
        containers
            .insert_leaf_with_parent_info(root, info, tile, activate)
            .expect("a live floating group must accept its incoming view");
        containers.layout_container_subtree(root);

        if activate {
            self.activate_window(containers, &win_id);
        }
    }

    /// Whether the window named here sits in a container a new sibling can join.
    ///
    /// About the named window, not the focused one: a floating window that is its own root has
    /// no container to join, whatever the focused window's answer is.
    pub(super) fn container_allows_splits(
        &self,
        containers: &ContainerTree<W>,
        id: &W::Id,
    ) -> bool {
        let tree = containers;
        let Some(key) = tree
            .window_key(id)
            .filter(|key| tree.workspace_of(*key) == Some(self.workspace))
        else {
            return false;
        };
        tree.container_of_allows_splits(key)
    }

    pub(super) fn container_pos(
        &self,
        containers: &ContainerTree<W>,
        id: &W::Id,
    ) -> Option<Point<f64, Logical>> {
        let idx = self.idx_of(containers, id)?;
        Some(self.container_area(containers, idx).loc)
    }

    pub(super) fn move_container_for_window_to(
        &mut self,
        containers: &mut ContainerTree<W>,
        id: &W::Id,
        pos: Point<f64, Logical>,
        animate: bool,
    ) -> bool {
        let Some(idx) = self.idx_of(containers, id) else {
            return false;
        };
        self.move_container_to(containers, idx, pos, animate);
        true
    }

    pub(super) fn add_tile_above(
        &mut self,
        containers: &mut ContainerTree<W>,
        above: &W::Id,
        mut tile: IncomingView<W>,
        activate: bool,
    ) {
        let idx = self.idx_of(containers, above).unwrap();

        let above_area = self.container_area(containers, idx);
        let tile_size = containers
            .incoming_tile(&tile)
            .expect("a floating insertion must own a valid incoming view")
            .tile_size();
        let pos =
            above_area.loc + (above_area.size.to_point() - tile_size.to_point()).downscale(2.);
        let pos =
            self.clamp_within_working_area(containers.working_area(self.workspace), pos, tile_size);
        containers
            .incoming_tile_mut(&mut tile)
            .expect("a floating insertion must own a valid incoming view")
            .floating_pos =
            Some(self.logical_to_size_frac(containers.working_area(self.workspace), pos));

        self.add_tile_at(containers, idx, tile, activate);
    }

    pub(super) fn add_subtree(
        &mut self,
        containers: &mut ContainerTree<W>,
        key: NodeKey,
        mut rect: Rectangle<f64, Logical>,
        workspace_floated: bool,
    ) -> bool {
        let config = containers.tile_config(self.workspace);
        let fullscreen_key = containers.fullscreen_key(self.workspace);
        let fullscreen_restore_size = fullscreen_key
            .filter(|fullscreen| *fullscreen == key)
            .and_then(|fullscreen| containers.fullscreen_restore_geometry(fullscreen))
            .map(|rect| rect.size);
        let mut prepared_leaf = None;
        let mut preserved_fullscreen_leaf = false;
        if containers.is_leaf(key) {
            if let Some(tile) = containers.get_tile_mut(key) {
                if fullscreen_key == Some(key)
                    && tile.window().pending_sizing_mode().is_fullscreen()
                {
                    // `container_set_floating(true)` moves the same fullscreen view between
                    // workspace lists. It neither revokes the workspace authority nor asks
                    // the client to return to normal. Its pre-fullscreen tiled box becomes
                    // the floating restore box when fullscreen is later disabled.
                    Self::preserve_fullscreen_tile_for_floating(
                        &config,
                        tile,
                        fullscreen_restore_size,
                    );
                    prepared_leaf = Some((key, None));
                    preserved_fullscreen_leaf = true;
                } else {
                    // `container_set_floating(true)` always runs Sway's natural-size pass for
                    // a view. A previous floating resize is pending geometry, not persistent
                    // restore state: after returning through tiled, enabling floating starts
                    // from the client's natural size again.
                    let size = tile.window().natural_size();
                    tile.floating_window_size =
                        Some(Self::floating_constraints(&config, tile, size));
                    let (_, requested) = Self::prepare_tile_for_floating(&config, tile);
                    prepared_leaf = Some((key, requested));
                }
            }
            if !preserved_fullscreen_leaf {
                let requested = prepared_leaf.and_then(|(_, requested)| requested);
                let working_area = containers.working_area(self.workspace);
                if let Some(tile) = containers.get_tile(key) {
                    let size = requested.unwrap_or_else(|| tile.tile_size());
                    let pos = self
                        .stored_or_default_tile_pos(working_area, tile)
                        .unwrap_or_else(|| center_preferring_top_left_in_area(working_area, size));
                    rect = Rectangle::new(pos, size);
                }
            }
        }

        let area = rect;
        let kind = if workspace_floated {
            FloatingRootKind::WorkspaceWrapper
        } else if prepared_leaf.is_some() {
            FloatingRootKind::ImplicitWindowGroup
        } else {
            FloatingRootKind::FloatedContainer
        };
        let workspace = self.workspace;
        let root = if workspace_floated {
            containers.float_whole_workspace(workspace, area, 0)
        } else {
            containers.float_as_group(key, area, 0, kind)
        };
        let Some(root) = root else {
            return false;
        };

        // `container_floating_resize_and_center` writes the new root's pending box
        // immediately, for a view and for a container alike. A different fullscreen branch
        // can make the following workspace arrange skip this floating branch; its descendants
        // then keep their old boxes, but the root itself already reports this one.
        containers.set_node_geometry(root, area);

        let keys = containers.leaf_keys_in_branch(root);
        for key in keys {
            if prepared_leaf.is_some_and(|(prepared, _)| prepared == key) {
                continue;
            }
            let fullscreen_restore_size = (fullscreen_key == Some(key))
                .then(|| containers.fullscreen_restore_geometry(key))
                .flatten()
                .map(|rect| rect.size);
            if let Some(tile) = containers.get_tile_mut(key) {
                if fullscreen_key == Some(key)
                    && tile.window().pending_sizing_mode().is_fullscreen()
                {
                    Self::preserve_fullscreen_tile_for_floating(
                        &config,
                        tile,
                        fullscreen_restore_size,
                    );
                } else {
                    Self::prepare_tile_for_floating(&config, tile);
                }
            }
        }
        let workspace = self.workspace;
        containers.layout(workspace);

        self.bring_up_descendants_of(containers, 0);
        true
    }

    fn bring_up_descendants_of(&mut self, containers: &mut ContainerTree<W>, idx: usize) {
        let tree = &*containers;
        let roots: Vec<_> = tree.floating_roots(self.workspace).collect();
        let base_windows = tree.windows_in_branch(roots[idx]);
        let mut seen_windows = base_windows;
        let mut descendants: Vec<usize> = Vec::new();

        for (i, root_below) in roots.iter().enumerate().skip(idx + 1).rev() {
            let windows = tree.windows_in_branch(*root_below);
            if windows
                .iter()
                .any(|win| seen_windows.iter().any(|parent| win.is_child_of(parent)))
            {
                descendants.push(i);
                seen_windows.extend(windows);
            }
        }

        let mut idx = idx;
        #[allow(clippy::explicit_counter_loop)]
        for descendant_idx in descendants.into_iter().rev() {
            self.raise_container(containers, descendant_idx, idx);
            idx += 1;
        }
    }

    pub(super) fn take_tile_for_transfer(
        &mut self,
        containers: &mut ContainerTree<W>,
        id: &W::Id,
    ) -> ViewTransfer {
        let idx = self.idx_of(containers, id).unwrap();
        self.take_tile_from_container(containers, idx, id)
    }

    pub(super) fn unfloat_container(
        &mut self,
        containers: &mut ContainerTree<W>,
        id: &W::Id,
        reference: Option<&InactiveTilingReference>,
        as_workspace: bool,
    ) -> bool {
        let Some(idx) = self.idx_of(containers, id) else {
            return false;
        };
        let root = self.root(containers, idx);

        if let Some(resize) = &self.interactive_resize {
            if self.idx_of(containers, &resize.window) == Some(idx) {
                self.interactive_resize = None;
            }
        }

        for tile in containers.tiles_in_branch_mut(root) {
            Self::store_floating_size_for_restore(tile);
        }
        let changed = if as_workspace {
            containers.unfloat_as_workspace(root)
        } else if let Some(reference) = reference {
            containers.unfloat_with_tiling_reference(root, reference)
        } else {
            containers.unfloat_into_workspace(root)
        };
        if !changed {
            return false;
        }
        let workspace = self.workspace;
        containers.layout(workspace);

        true
    }

    pub(super) fn unfloat_window(
        &mut self,
        containers: &mut ContainerTree<W>,
        id: &W::Id,
        reference: Option<&InactiveTilingReference>,
    ) -> bool {
        let Some(idx) = self.idx_of(containers, id) else {
            return false;
        };
        let Some(key) = containers.window_key(id) else {
            return false;
        };
        let pos = containers
            .floating_position(self.root(containers, idx))
            .expect("every floating stack entry must name a floating root");
        if let Some(tile) = containers.get_tile_mut(key) {
            Self::store_floating_size_for_restore(tile);
            tile.floating_pos = Some(pos);
            tile.set_scratchpad(false);
        }
        let Some(group_empty) = containers.unfloat_node(key, reference) else {
            return false;
        };
        let _ = group_empty;
        let workspace = self.workspace;
        containers.layout(workspace);
        if self
            .interactive_resize
            .as_ref()
            .is_some_and(|resize| &resize.window == id)
        {
            self.interactive_resize = None;
        }
        true
    }

    pub(super) fn active_container_is_workspace_floated(
        &self,
        containers: &ContainerTree<W>,
    ) -> bool {
        containers
            .active_floating_window_id(self.workspace)
            .as_ref()
            .and_then(|id| self.idx_of(containers, id))
            .is_some_and(|idx| containers.floating_root_is_workspace_wrapper(self.workspace, idx))
    }

    fn take_tile_from_container(
        &mut self,
        containers: &mut ContainerTree<W>,
        idx: usize,
        id: &W::Id,
    ) -> ViewTransfer {
        let root = self.root(containers, idx);
        let container_pos = containers
            .floating_position(root)
            .expect("every floating stack entry must name a floating root");
        let container_id = containers
            .floating_root_id_at(self.workspace, idx)
            .expect("floating index must have a stable reinsertion id");
        let insert_hint = containers.insert_parent_info_for_window(id);
        let workspace = self.workspace;
        let node = containers
            .take_window_for_transfer(workspace, id)
            .expect("window must exist in floating container");

        // Stop interactive resize.
        if let Some(resize) = &self.interactive_resize {
            if id == &resize.window {
                self.interactive_resize = None;
            }
        }

        // An implicit single-window group is registered under the window's own node, so once
        // that node is transferred the root has left with it and the branch count cannot say so:
        // it still counts the transferred node's own window, because transferring keeps the node.
        // Leaving the stack entry behind would mean the next registration of the same key finds
        // it already there.
        if node != root && containers.window_count_in_branch(root) == 0 {
            assert!(containers.remove_empty_floating_root(root));
        }

        let tile = containers
            .transferred_tile_mut(node)
            .expect("the floating view was just transferred");
        Self::store_floating_size_for_restore(tile);
        // Store the floating position.
        tile.floating_pos = Some(container_pos);
        tile.floating_reinsert_hint = Some((container_id, insert_hint));

        ViewTransfer {
            node,
            is_floating: true,
        }
    }

    fn store_floating_size_for_restore(tile: &mut Tile<W>) {
        let window = tile.window();
        let can_restore_current_size = window.pending_sizing_mode().is_normal();
        if can_restore_current_size {
            tile.floating_window_size = Some(tile.window_expected_or_current_size().to_i32_round());
        }
    }

    pub fn start_close_animation_for_window(
        &mut self,
        containers: &mut ContainerTree<W>,
        renderer: &mut GlesRenderer,
        id: &W::Id,
        blocker: TransactionBlocker,
    ) {
        let (tile, tile_pos) = containers
            .floating_tiles_with_render_positions_mut(self.workspace, false)
            .find(|(tile, _)| tile.window().id() == id)
            .unwrap();

        let Some(snapshot) = tile.take_unmap_snapshot() else {
            return;
        };

        let tile_size = tile.tile_size();

        self.start_close_animation_for_tile(
            containers, renderer, snapshot, tile_size, tile_pos, blocker,
        );
    }

    pub fn activate_window_without_raising(
        &mut self,
        containers: &mut ContainerTree<W>,
        id: &W::Id,
    ) -> bool {
        let Some(_idx) = self.idx_of(containers, id) else {
            return false;
        };

        containers.clear_selection();
        let _ = containers.focus_window_by_id(self.workspace, id);
        true
    }

    pub fn activate_window(&mut self, containers: &mut ContainerTree<W>, id: &W::Id) -> bool {
        let Some(idx) = self.idx_of(containers, id) else {
            return false;
        };

        self.raise_container(containers, idx, 0);
        self.bring_up_descendants_of(containers, 0);
        containers.clear_selection();
        let _ = containers.focus_window_by_id(self.workspace, id);

        true
    }

    fn raise_container(
        &mut self,
        containers: &mut ContainerTree<W>,
        from_idx: usize,
        to_idx: usize,
    ) {
        assert!(to_idx <= from_idx);
        assert!(containers.move_floating_root(self.workspace, from_idx, to_idx));
    }

    pub fn start_close_animation_for_tile(
        &mut self,
        containers: &ContainerTree<W>,
        renderer: &mut GlesRenderer,
        snapshot: TileRenderSnapshot,
        tile_size: Size<f64, Logical>,
        tile_pos: Point<f64, Logical>,
        blocker: TransactionBlocker,
    ) {
        let anim = Animation::new(
            containers.clock().clone(),
            0.,
            1.,
            0.,
            containers
                .options(self.workspace)
                .animations
                .window_close
                .anim,
        );

        let scale = Scale::from(containers.scale(self.workspace));
        let res = ClosingWindow::new(
            renderer, snapshot, scale, tile_size, tile_pos, blocker, anim,
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

    fn resolve_target_id(
        &self,
        containers: &ContainerTree<W>,
        id: Option<&W::Id>,
    ) -> Option<W::Id> {
        id.cloned()
            .or_else(|| containers.active_floating_window_id(self.workspace))
    }

    fn next_preset_idx(
        presets: &[PresetSize],
        available_size: f64,
        forwards: bool,
        current_window: f64,
        current_tile: f64,
        current_idx: Option<usize>,
    ) -> usize {
        let len = presets.len();
        if let Some(idx) = current_idx {
            (idx + if forwards { 1 } else { len - 1 }) % len
        } else {
            let mut it = presets
                .iter()
                .map(|preset| resolve_preset_size(*preset, available_size));

            if forwards {
                it.position(|resolved| {
                    match resolved {
                        // Some allowance for fractional scaling purposes.
                        ResolvedSize::Tile(resolved) => current_tile + 1. < resolved,
                        ResolvedSize::Window(resolved) => current_window + 1. < resolved,
                    }
                })
                .unwrap_or(0)
            } else {
                it.rposition(|resolved| {
                    match resolved {
                        // Some allowance for fractional scaling purposes.
                        ResolvedSize::Tile(resolved) => resolved + 1. < current_tile,
                        ResolvedSize::Window(resolved) => resolved + 1. < current_window,
                    }
                })
                .unwrap_or(len - 1)
            }
        }
    }

    pub fn toggle_window_width(
        &mut self,
        containers: &mut ContainerTree<W>,
        id: Option<&W::Id>,
        forwards: bool,
    ) {
        let Some(id) = self.resolve_target_id(containers, id) else {
            return;
        };
        let available_size = containers.working_area(self.workspace).size.w;
        let presets = containers
            .options(self.workspace)
            .layout
            .preset_column_widths
            .clone();

        let Some(tile) = self.tile_at_mut(containers, &id) else {
            return;
        };
        let current_window = tile.window_expected_or_current_size().w;
        let current_tile = tile.tile_expected_or_current_size().w;
        let preset_idx = Self::next_preset_idx(
            &presets,
            available_size,
            forwards,
            current_window,
            current_tile,
            tile.floating_preset_width_idx,
        );

        let preset = presets[preset_idx];
        self.set_window_width(containers, Some(&id), SizeChange::from(preset), true);

        if let Some(tile) = self.tile_at_mut(containers, &id) {
            tile.floating_preset_width_idx = Some(preset_idx);
        }

        self.interactive_resize_end(Some(&id));
    }

    pub fn start_open_animation(&self, containers: &mut ContainerTree<W>, id: &W::Id) -> bool {
        if let Some(tile) = self.tile_at_mut(containers, id) {
            tile.start_open_animation();
            true
        } else {
            false
        }
    }

    pub fn toggle_window_height(
        &mut self,
        containers: &mut ContainerTree<W>,
        id: Option<&W::Id>,
        forwards: bool,
    ) {
        let Some(id) = self.resolve_target_id(containers, id) else {
            return;
        };
        let available_size = containers.working_area(self.workspace).size.h;
        let presets = containers
            .options(self.workspace)
            .layout
            .preset_window_heights
            .clone();

        let Some(tile) = self.tile_at_mut(containers, &id) else {
            return;
        };
        let current_window = tile.window_expected_or_current_size().h;
        let current_tile = tile.tile_expected_or_current_size().h;
        let preset_idx = Self::next_preset_idx(
            &presets,
            available_size,
            forwards,
            current_window,
            current_tile,
            tile.floating_preset_height_idx,
        );

        let preset = presets[preset_idx];
        self.set_window_height(containers, Some(&id), SizeChange::from(preset), true);

        if let Some(tile) = self.tile_at_mut(containers, &id) {
            tile.floating_preset_height_idx = Some(preset_idx);
        }

        self.interactive_resize_end(Some(&id));
    }

    fn resize_container_dimension(
        &mut self,
        containers: &mut ContainerTree<W>,
        idx: usize,
        change: SizeChange,
        axis: ResizeAxis,
        anchor: FloatingResizeAnchor,
    ) {
        let is_width = axis == ResizeAxis::Horizontal;
        let available = if is_width {
            containers.working_area(self.workspace).size.w
        } else {
            containers.working_area(self.workspace).size.h
        };
        let root = self.root(containers, idx);
        // sway edits the floating container's current pending box. While that same root owns
        // workspace fullscreen, the current box is output-sized rather than the saved
        // pre-fullscreen floating box.
        let current_area = if containers.fullscreen_key(self.workspace) == Some(root) {
            containers
                .node_geometry(root)
                .unwrap_or_else(|| self.container_area(containers, idx))
        } else {
            self.container_area(containers, idx)
        };
        let current = if is_width {
            current_area.size.w
        } else {
            current_area.size.h
        };

        const MAX_F: f64 = 10000.;

        let current_px = current.round().clamp(0.0, i32::MAX as f64) as i32;
        let new_size = match change {
            SizeChange::SetFixed(value) => f64::from(value),
            SizeChange::SetProportion(prop) => {
                let prop = (prop / 100.).clamp(0., MAX_F);
                available * prop
            }
            SizeChange::AdjustFixed(delta) => f64::from(current_px.saturating_add(delta)),
            SizeChange::AdjustProportion(delta) => {
                let current_prop = current / available.max(1.0);
                let prop = (current_prop + delta / 100.).clamp(0., MAX_F);
                available * prop
            }
        }
        .round()
        .clamp(
            if is_width { 75. } else { 50. },
            if is_width {
                containers.view_size(self.workspace).w
            } else {
                containers.view_size(self.workspace).h
            },
        );

        let size = if is_width {
            Size::from((new_size, current_area.size.h))
        } else {
            Size::from((current_area.size.w, new_size))
        };
        self.set_container_size(containers, idx, size);
        if anchor == FloatingResizeAnchor::Center {
            let centered_pos = Point::from((
                current_area.loc.x + (current_area.size.w - size.w) / 2.,
                current_area.loc.y + (current_area.size.h - size.h) / 2.,
            ));
            self.set_container_logical_pos(containers, idx, centered_pos);
        }
        let area = self.container_area(containers, idx);
        containers.layout_container_subtree_in(root, area);
    }

    fn resize_container_around_center(
        &mut self,
        containers: &mut ContainerTree<W>,
        idx: usize,
        change: SizeChange,
        axis: ResizeAxis,
    ) {
        self.resize_container_dimension(
            containers,
            idx,
            change,
            axis,
            FloatingResizeAnchor::Center,
        );
    }

    pub fn set_window_width(
        &mut self,
        containers: &mut ContainerTree<W>,
        id: Option<&W::Id>,
        change: SizeChange,
        animate: bool,
    ) {
        let active = containers.active_floating_window_id(self.workspace);
        let Some(target_id) = id.or(active.as_ref()) else {
            return;
        };
        let idx = self.idx_of(containers, target_id).unwrap();
        let selection_is_container = id.is_none() && self.selected_is_container_in(containers, idx);
        if selection_is_container {
            self.resize_container_around_center(containers, idx, change, ResizeAxis::Horizontal);
            return;
        }

        let key = if let Some(id) = id {
            match containers.window_key(id) {
                Some(key) => key,
                None => return,
            }
        } else {
            match self.selected_key_in(containers, idx) {
                Some(key) => key,
                None => return,
            }
        };

        if let Some(tile) = containers.get_tile_mut(key) {
            tile.floating_preset_width_idx = None;
        }

        let Some((parent_key, child_idx, available, child_count, _)) =
            containers.container_metrics_for(key, LayoutMode::SplitH)
        else {
            self.resize_container_around_center(containers, idx, change, ResizeAxis::Horizontal);
            return;
        };
        if child_count <= 1 {
            self.resize_container_around_center(containers, idx, change, ResizeAxis::Horizontal);
            return;
        }

        let current_percent = containers
            .child_percent(parent_key, child_idx)
            .unwrap_or(1.0);
        let percent = percent_from_size_change(
            current_percent,
            available,
            || containers.ppt_reference(key, LayoutMode::SplitH),
            change,
        );

        if containers.set_child_percent(parent_key, child_idx, LayoutMode::SplitH, percent) {
            let _ = animate;
            let root = self.root(containers, idx);
            containers.layout_branch(root);
        }
    }

    /// Apply a keyboard/IPC resize to a floating target.
    ///
    /// Axis requests change the size directly. Edge requests reuse the same geometry operation as
    /// an edge drag, including anchoring the opposite edge, but remain a one-shot command rather
    /// than becoming interactive state owned by the input layer.
    pub fn resize_window(
        &mut self,
        containers: &mut ContainerTree<W>,
        id: Option<&W::Id>,
        request: ResizeRequest,
    ) {
        match request {
            ResizeRequest::Axis {
                axis: ResizeAxis::Horizontal,
                change,
            } => self.set_window_width(containers, id, change, true),
            ResizeRequest::Axis {
                axis: ResizeAxis::Vertical,
                change,
            } => self.set_window_height(containers, id, change, true),
            ResizeRequest::Edge { direction, amount } => {
                self.resize_window_edge(containers, id, direction, amount)
            }
        }
    }

    fn resize_window_edge(
        &mut self,
        containers: &mut ContainerTree<W>,
        id: Option<&W::Id>,
        direction: Direction,
        amount: i32,
    ) {
        let Some(id) = self.resolve_target_id(containers, id) else {
            return;
        };
        let Some(idx) = self.idx_of(containers, &id) else {
            return;
        };
        let root = self.root(containers, idx);
        let current = if containers.fullscreen_key(self.workspace) == Some(root) {
            containers
                .node_geometry(root)
                .unwrap_or_else(|| self.container_area(containers, idx))
        } else {
            self.container_area(containers, idx)
        };
        let amount = f64::from(amount);
        let horizontal = matches!(direction, Direction::Left | Direction::Right);
        let current_dimension = if horizontal {
            current.size.w
        } else {
            current.size.h
        };
        let target_dimension = (current_dimension + amount).round().clamp(
            if horizontal { 75. } else { 50. },
            if horizontal {
                containers.view_size(self.workspace).w
            } else {
                containers.view_size(self.workspace).h
            },
        );
        let effective_growth = target_dimension - current_dimension;
        if effective_growth == 0. {
            return;
        }

        let mut size = current.size;
        let mut pos = current.loc;
        if horizontal {
            size.w = target_dimension;
            if direction == Direction::Left {
                pos.x -= effective_growth;
            }
        } else {
            size.h = target_dimension;
            if direction == Direction::Up {
                pos.y -= effective_growth;
            }
        }
        self.set_container_size(containers, idx, size);
        self.set_container_logical_pos(containers, idx, pos);
        let area = self.container_area(containers, idx);
        containers.layout_container_subtree_in(root, area);
    }

    pub fn set_window_height(
        &mut self,
        containers: &mut ContainerTree<W>,
        id: Option<&W::Id>,
        change: SizeChange,
        animate: bool,
    ) {
        let active = containers.active_floating_window_id(self.workspace);
        let Some(target_id) = id.or(active.as_ref()) else {
            return;
        };
        let idx = self.idx_of(containers, target_id).unwrap();
        let selection_is_container = id.is_none() && self.selected_is_container_in(containers, idx);
        if selection_is_container {
            self.resize_container_around_center(containers, idx, change, ResizeAxis::Vertical);
            return;
        }

        let key = if let Some(id) = id {
            match containers.window_key(id) {
                Some(key) => key,
                None => return,
            }
        } else {
            match self.selected_key_in(containers, idx) {
                Some(key) => key,
                None => return,
            }
        };

        if let Some(tile) = containers.get_tile_mut(key) {
            tile.floating_preset_height_idx = None;
        }

        let Some((parent_key, child_idx, available, child_count, _)) =
            containers.container_metrics_for(key, LayoutMode::SplitV)
        else {
            self.resize_container_around_center(containers, idx, change, ResizeAxis::Vertical);
            return;
        };
        if child_count <= 1 {
            self.resize_container_around_center(containers, idx, change, ResizeAxis::Vertical);
            return;
        }

        let current_percent = containers
            .child_percent(parent_key, child_idx)
            .unwrap_or(1.0);
        let percent = percent_from_size_change(
            current_percent,
            available,
            || containers.ppt_reference(key, LayoutMode::SplitV),
            change,
        );

        if containers.set_child_percent(parent_key, child_idx, LayoutMode::SplitV, percent) {
            let _ = animate;
            let root = self.root(containers, idx);
            containers.layout_branch(root);
        }
    }

    fn directional_root(
        &self,
        containers: &ContainerTree<W>,
        distance: impl Fn(Point<f64, Logical>, Point<f64, Logical>) -> f64,
    ) -> Option<NodeKey> {
        let active_idx = self.active_container_idx(containers)?;
        let active_area = self.container_area(containers, active_idx);
        let center = active_area.loc + active_area.size.downscale(2.);

        let roots: Vec<_> = containers.floating_roots(self.workspace).collect();
        roots
            .into_iter()
            .enumerate()
            // sway walks `ws->floating` bottom-to-top; Tiri stores render order top-to-bottom.
            .rev()
            .filter(|(idx, _)| *idx != active_idx)
            .map(|(idx, root)| {
                let area = self.container_area(containers, idx);
                let other_center = area.loc + area.size.downscale(2.);
                (root, distance(center, other_center))
            })
            // sway accepts a zero signed distance. Overlapping floating roots therefore
            // focus the first other entry in workspace stack order instead of becoming a
            // directional no-op.
            .filter(|(_, dist)| *dist >= 0.)
            .min_by(|(_, dist_a), (_, dist_b)| f64::total_cmp(dist_a, dist_b))
            .map(|(root, _)| root)
    }

    /// Focus one `workspace->floating` entry and raise that entry exactly once.
    ///
    /// `descend` is false when sway's directional search returned the floating root itself,
    /// and true when a tiling-style sibling step asks for its inactive view. A lone view is
    /// both the root and the leaf, so the distinction only becomes visible for groups.
    fn focus_root_at(
        &mut self,
        containers: &mut ContainerTree<W>,
        idx: usize,
        descend: bool,
    ) -> bool {
        let Some(root) = containers.floating_root_at(self.workspace, idx) else {
            return false;
        };
        let landing = descend
            .then(|| containers.focus_inactive_view(root))
            .flatten();

        self.raise_container(containers, idx, 0);
        self.bring_up_descendants_of(containers, 0);

        if let Some(landing) = landing {
            containers.focus_node(landing);
        } else if containers.branch_root_is_lone_window(root) {
            containers.focus_node(root);
        } else {
            containers.select_container(root);
        }
        true
    }

    fn focus_directional(
        &mut self,
        containers: &mut ContainerTree<W>,
        distance: impl Fn(Point<f64, Logical>, Point<f64, Logical>) -> f64,
    ) -> bool {
        let Some(root) = self.directional_root(containers, distance) else {
            return false;
        };
        let Some(idx) = containers.floating_root_index(self.workspace, root) else {
            return false;
        };
        self.focus_root_at(containers, idx, true)
    }

    /// sway's `focus next|prev [sibling]` from the floating side.
    pub(super) fn focus_along_parent(
        &mut self,
        containers: &mut ContainerTree<W>,
        forward: bool,
        descend: bool,
    ) -> bool {
        let Some(active_idx) = self.active_container_idx(containers) else {
            return false;
        };
        let root = self.root(containers, active_idx);
        let Some(position) = self.selected_key_in(containers, active_idx) else {
            return false;
        };
        let Some(parent_layout) = containers.command_target_parent_layout(self.workspace) else {
            return false;
        };
        let direction = match (parent_layout.is_horizontal(), forward) {
            (true, true) => Direction::Right,
            (true, false) => Direction::Left,
            (false, true) => Direction::Down,
            (false, false) => Direction::Up,
        };

        // The shared tree traversal owns every step inside the floating subtree.
        if containers.focus_along_parent(self.workspace, forward, descend) {
            return true;
        }
        if containers.fullscreen_key(self.workspace).is_some() || self.root_count(containers) <= 1 {
            return false;
        }

        if position == root {
            // Starting on a top-level floating container uses sway's geometric floating
            // search. It does not wrap and returns the root node rather than descending.
            let candidate = match direction {
                Direction::Left => {
                    self.directional_root(containers, |focus, other| focus.x - other.x)
                }
                Direction::Right => {
                    self.directional_root(containers, |focus, other| other.x - focus.x)
                }
                Direction::Up => {
                    self.directional_root(containers, |focus, other| focus.y - other.y)
                }
                Direction::Down => {
                    self.directional_root(containers, |focus, other| other.y - focus.y)
                }
            };
            let Some(candidate) = candidate else {
                return false;
            };
            let Some(idx) = containers.floating_root_index(self.workspace, candidate) else {
                return false;
            };
            return self.focus_root_at(containers, idx, false);
        }

        // Starting below a floating root follows the ordinary tiling walk, which climbs out
        // of the branch into the list of floating roots.
        self.step_root_sibling(containers, active_idx, direction, descend, true)
    }

    /// sway's sibling step at the floating root level.
    ///
    /// A top-level floating container has no parent, so `container_get_siblings` hands it
    /// `workspace->floating` and `container_parent_layout` hands it the workspace's own
    /// layout: climbing out of a floating branch lands in the list of floating roots, laid
    /// out the way the workspace is. That list runs bottom-to-top and Tiri's render stack is
    /// the reverse, which is what the leading/trailing flip below is about.
    ///
    /// A direct landing may keep the root selected — that is `focus next sibling` — while a
    /// wrap always descends, because sway resolves its wrap candidate through
    /// `seat_get_focus_inactive_view`.
    fn step_root_sibling(
        &mut self,
        containers: &mut ContainerTree<W>,
        active_idx: usize,
        direction: Direction,
        descend: bool,
        allow_wrap: bool,
    ) -> bool {
        if self.root_count(containers) <= 1 {
            return false;
        }
        let workspace = self.workspace;
        if !containers
            .workspace_layout(workspace)
            .is_parallel_to(direction)
        {
            return false;
        }
        let direct = if direction.is_leading() {
            active_idx
                .checked_add(1)
                .filter(|idx| *idx < self.root_count(containers))
        } else {
            active_idx.checked_sub(1)
        };
        let (target_idx, wrapped) = match direct {
            Some(idx) => (idx, false),
            None if !allow_wrap => return false,
            None if direction.is_leading() => (0, true),
            None => (self.root_count(containers) - 1, true),
        };
        self.focus_root_at(containers, target_idx, descend || wrapped)
    }

    fn focus_within_active_container(
        &mut self,
        containers: &mut ContainerTree<W>,
        direction: Direction,
        allow_wrap: bool,
    ) -> bool {
        let Some(idx) = self.active_container_idx(containers) else {
            return false;
        };
        let branch_root = self.root(containers, idx);
        if containers.focus_in_direction_in_branch(branch_root, direction, allow_wrap) {
            return true;
        }

        // The walk ran out of branch without finding anything. sway does not stop there:
        // `node_get_in_direction_tiling` climbs to the floating root through
        // `pending.parent` and takes one more step along `workspace->floating`. Only from
        // below, though — a command aimed at the root itself is the one case sway sends to
        // the geometric floating search instead, and that is the caller's fallback.
        if containers.branch_position(branch_root) == Some(branch_root) {
            return false;
        }
        if containers.fullscreen_key(self.workspace).is_some() {
            return false;
        }
        self.step_root_sibling(containers, idx, direction, true, allow_wrap)
    }

    fn focus_in_stack_order(&mut self, containers: &mut ContainerTree<W>, delta: isize) -> bool {
        if self.root_count(containers) <= 1 {
            return false;
        }

        let Some(idx) = self.active_container_idx(containers) else {
            return false;
        };
        let len = self.root_count(containers) as isize;
        let target_idx = (idx as isize + delta).rem_euclid(len) as usize;
        if target_idx == idx {
            return false;
        }

        let root = self.root(containers, target_idx);
        let Some(id) = containers
            .focused_window_in_branch(root)
            .map(|win| win.id().clone())
            .or_else(|| {
                containers
                    .windows_in_branch(root)
                    .into_iter()
                    .next()
                    .map(|win| win.id().clone())
            })
        else {
            return false;
        };

        self.activate_window(containers, &id);
        true
    }

    fn focus_in_stable_container_order(
        &mut self,
        containers: &mut ContainerTree<W>,
        descending: bool,
    ) -> bool {
        if self.root_count(containers) <= 1 {
            return false;
        }

        let Some(active_idx) = self.active_container_idx(containers) else {
            return false;
        };

        let mut ordered: Vec<_> = (0..self.root_count(containers))
            .map(|idx| {
                (
                    idx,
                    containers
                        .floating_root_id_at(self.workspace, idx)
                        .expect("floating root must have a stable id"),
                )
            })
            .collect();
        ordered.sort_by_key(|(_, id)| *id);
        if descending {
            ordered.reverse();
        }

        let Some(pos) = ordered.iter().position(|(idx, _)| *idx == active_idx) else {
            return false;
        };
        let target_idx = ordered[(pos + 1) % ordered.len()].0;
        if target_idx == active_idx {
            return false;
        }

        let root = self.root(containers, target_idx);
        let Some(id) = containers
            .focused_window_in_branch(root)
            .map(|win| win.id().clone())
            .or_else(|| {
                containers
                    .windows_in_branch(root)
                    .into_iter()
                    .next()
                    .map(|win| win.id().clone())
            })
        else {
            return false;
        };

        self.activate_window(containers, &id);
        true
    }

    fn should_cycle_top_level_stable_order(&self, containers: &ContainerTree<W>) -> bool {
        self.root_count(containers) > 1
            && self.active_container_idx(containers).is_some_and(|idx| {
                containers.branch_root_is_lone_window(self.root(containers, idx))
            })
            && containers
                .floating_roots(self.workspace)
                .all(|root| containers.branch_root_is_lone_window(root))
    }

    /// sway's `focus <direction>` from the floating side; `wrap` is `focus_wrapping`.
    ///
    /// Inside a floating group the tree walks the group as it walks the tiled side. Between
    /// groups sway searches geometrically, except that a horizontal step with wrapping cycles
    /// through the floating stack first.
    pub(super) fn focus_in_direction(
        &mut self,
        containers: &mut ContainerTree<W>,
        direction: Direction,
        wrap: bool,
    ) -> bool {
        if self.focus_within_active_container(containers, direction, wrap) {
            return true;
        }
        if containers.fullscreen_key(self.workspace).is_some() {
            return false;
        }
        if wrap && direction.is_horizontal() {
            let leading = direction == Direction::Left;
            if self.should_cycle_top_level_stable_order(containers) {
                return self.focus_in_stable_container_order(containers, leading);
            }
            if self.focus_in_stack_order(containers, if leading { 1 } else { -1 }) {
                return true;
            }
        }
        self.focus_directional(containers, |focus, other| match direction {
            Direction::Left => focus.x - other.x,
            Direction::Right => other.x - focus.x,
            Direction::Up => focus.y - other.y,
            Direction::Down => other.y - focus.y,
        })
    }

    /// Focus the window furthest towards `direction`.
    pub(super) fn focus_edge(&mut self, containers: &mut ContainerTree<W>, direction: Direction) {
        let coordinate = |pos: &Point<f64, Logical>| {
            if direction.is_horizontal() {
                pos.x
            } else {
                pos.y
            }
        };
        let tiles = self.tiles_with_offsets_visible(containers);
        let result = if direction.is_leading() {
            tiles.min_by(|(_, a), (_, b)| f64::total_cmp(&coordinate(a), &coordinate(b)))
        } else {
            tiles.max_by(|(_, a), (_, b)| f64::total_cmp(&coordinate(a), &coordinate(b)))
        };
        if let Some((tile, _)) = result {
            let id = tile.window().id().clone();
            self.activate_window(containers, &id);
        }
    }

    pub(super) fn focus_parent(&mut self, containers: &mut ContainerTree<W>) -> bool {
        let Some(idx) = self.active_container_idx(containers) else {
            return false;
        };
        if containers.focus_parent_is_obstructed_by_fullscreen(self.workspace) {
            return true;
        }
        let root = self.root(containers, idx);
        if containers.handle_focus_parent_in_branch_fullscreen_scope(root) {
            return true;
        }

        // One step, not a walk: `select_parent_in` stops at the branch root, so there is
        // never a second ancestor to consider inside the group.
        if containers.select_parent_in(root) {
            return true;
        }

        // It declined, so the position already *is* the root, and above a floating root is
        // the workspace — which Workspace represents outside ContainerTree. A lone floating
        // window is its own root, so this is the first step from it too: sway has no extra
        // stop for a view that floats, because there is no container around it to stop on.
        let workspace = self.workspace;
        containers.select_parent(workspace)
    }

    pub fn focus_child(&mut self, containers: &mut ContainerTree<W>) -> bool {
        let Some(idx) = self.active_container_idx(containers) else {
            return false;
        };
        let root = self.root(containers, idx);
        let workspace = self.workspace;
        containers
            .selected_container_key()
            .is_some_and(|key| containers.is_descendant(key, root))
            && containers.select_child(workspace)
    }

    fn move_container_to(
        &mut self,
        containers: &mut ContainerTree<W>,
        idx: usize,
        new_pos: Point<f64, Logical>,
        animate: bool,
    ) {
        if animate {
            self.move_container_and_animate(containers, idx, new_pos);
        } else {
            self.set_container_logical_pos(containers, idx, new_pos);
        }

        let root = self.root(containers, idx);
        containers.layout_branch(root);

        self.interactive_resize_end(None);
    }

    fn move_by(&mut self, containers: &mut ContainerTree<W>, amount: Point<f64, Logical>) {
        let Some(active_id) = containers.active_floating_window_id(self.workspace) else {
            return;
        };
        if containers.floating_window_owns_fullscreen(self.workspace, &active_id) {
            return;
        }
        let idx = self.idx_of(containers, &active_id).unwrap();

        let new_pos = self.container_area(containers, idx).loc + amount;
        self.move_container_to(containers, idx, new_pos, true)
    }

    /// sway's `move <direction>` for a floating root: a translation by
    /// [`DIRECTIONAL_MOVE_PX`].
    pub(super) fn move_in_direction(
        &mut self,
        containers: &mut ContainerTree<W>,
        direction: Direction,
    ) {
        let delta = match direction {
            Direction::Left => Point::from((-DIRECTIONAL_MOVE_PX, 0.)),
            Direction::Right => Point::from((DIRECTIONAL_MOVE_PX, 0.)),
            Direction::Up => Point::from((0., -DIRECTIONAL_MOVE_PX)),
            Direction::Down => Point::from((0., DIRECTIONAL_MOVE_PX)),
        };
        self.move_by(containers, delta);
    }

    pub fn move_window(
        &mut self,
        containers: &mut ContainerTree<W>,
        id: Option<&W::Id>,
        x: PositionChange,
        y: PositionChange,
        animate: bool,
    ) {
        let Some(id) = self.resolve_target_id(containers, id) else {
            return;
        };
        if containers.floating_window_owns_fullscreen(self.workspace, &id) {
            return;
        }
        let idx = self.idx_of(containers, &id).unwrap();

        let mut pos = self.container_area(containers, idx).loc;

        let available_width = containers.working_area(self.workspace).size.w;
        let available_height = containers.working_area(self.workspace).size.h;
        let working_area_loc = containers.working_area(self.workspace).loc;

        const MAX_F: f64 = 10000.;

        match x {
            PositionChange::SetFixed(x) => pos.x = x + working_area_loc.x,
            PositionChange::SetProportion(prop) => {
                let prop = (prop / 100.).clamp(0., MAX_F);
                pos.x = available_width * prop + working_area_loc.x;
            }
            PositionChange::AdjustFixed(x) => pos.x += x,
            PositionChange::AdjustProportion(prop) => {
                let current_prop = (pos.x - working_area_loc.x) / available_width.max(1.);
                let prop = (current_prop + prop / 100.).clamp(0., MAX_F);
                pos.x = available_width * prop + working_area_loc.x;
            }
        }
        match y {
            PositionChange::SetFixed(y) => pos.y = y + working_area_loc.y,
            PositionChange::SetProportion(prop) => {
                let prop = (prop / 100.).clamp(0., MAX_F);
                pos.y = available_height * prop + working_area_loc.y;
            }
            PositionChange::AdjustFixed(y) => pos.y += y,
            PositionChange::AdjustProportion(prop) => {
                let current_prop = (pos.y - working_area_loc.y) / available_height.max(1.);
                let prop = (current_prop + prop / 100.).clamp(0., MAX_F);
                pos.y = available_height * prop + working_area_loc.y;
            }
        }

        self.move_container_to(containers, idx, pos, animate);
    }

    /// Center one floating root in the working area, as sway's
    /// `container_floating_move_to_center` does.
    pub(super) fn center_root(&mut self, containers: &mut ContainerTree<W>, root: NodeKey) {
        let Some(idx) = containers.floating_root_index(self.workspace, root) else {
            return;
        };
        let new_pos = center_preferring_top_left_in_area(
            containers.working_area(self.workspace),
            self.container_area(containers, idx).size,
        );
        self.move_container_to(containers, idx, new_pos, false);
    }

    pub fn center_window(&mut self, containers: &mut ContainerTree<W>, id: Option<&W::Id>) {
        let Some(id) = self.resolve_target_id(containers, id) else {
            return;
        };
        if containers.floating_window_owns_fullscreen(self.workspace, &id) {
            return;
        }
        let idx = self.idx_of(containers, &id).unwrap();

        let new_pos = center_preferring_top_left_in_area(
            containers.working_area(self.workspace),
            self.container_area(containers, idx).size,
        );
        self.move_container_to(containers, idx, new_pos, true);
    }

    pub fn descendants_added(&mut self, containers: &mut ContainerTree<W>, id: &W::Id) -> bool {
        let Some(idx) = self.idx_of(containers, id) else {
            return false;
        };

        self.bring_up_descendants_of(containers, idx);
        true
    }

    pub fn update_window(
        &mut self,
        containers: &mut ContainerTree<W>,
        id: &W::Id,
        serial: Option<Serial>,
    ) -> bool {
        let Some(container_idx) = self.idx_of(containers, id) else {
            return false;
        };

        {
            let Some(key) = containers.window_key(id) else {
                return false;
            };
            let Some(tile) = containers.get_tile_mut(key) else {
                return false;
            };

            // Do this before calling update_window() so it can get up-to-date info.
            if let Some(serial) = serial {
                tile.window_mut().on_commit(serial);
            }

            if let Some(resize) = &self.interactive_resize {
                if id == &resize.window {
                    tile.window_mut().set_interactive_resize(Some(resize.data));
                    tile.stop_move_animations();
                    tile.clear_resize_animation();
                }
            }

            tile.update_window();
        }

        let root = self.root(containers, container_idx);
        // A surface commit acknowledges (or independently changes) that view's content box;
        // it is not an `arrange_workspace` call. The pending branch layout is applied by the
        // transaction path. Arranging again here is observably wrong after `view_map` has
        // directly arranged a floating parent containing the workspace-fullscreen view: the
        // acknowledgement of its new sibling would expand that old view back to the output.
        //
        // sway/desktop/xdg_shell.c:291-344
        // sway/tree/view.c:979-984

        // Only a root which is itself the view can take its resize base from that view's
        // commit. An explicit one-view tabbed/stacked/split container includes decoration and
        // owns an independent outer box; feeding its child's content size back here erases
        // that decoration from the container on every commit.
        if containers.branch_root_is_lone_window(root) {
            let Some(key) = containers.window_key(id) else {
                return true;
            };
            let Some(tile) = containers.get_tile(key) else {
                return true;
            };
            // Fullscreen is a temporary output-sized commit, not a new floating resize base.
            // Keeping it would make unfullscreen restore the output size instead of the last
            // ordinary floating size.
            if tile.window().pending_sizing_mode().is_normal() {
                let tile_size = tile.tile_size();
                containers.record_floating_resize_base(root, tile_size);
            }
        }

        true
    }

    fn render_elements<R: NiriRenderer>(
        &self,
        containers: &ContainerTree<W>,
        mut ctx: RenderCtx<R>,
        xray_pos: XrayPos,
        view_rect: Rectangle<f64, Logical>,
        params: BranchRenderParams,
        roots: FloatingRoots,
    ) -> Vec<FloatingRenderElement<R>> {
        let BranchRenderParams {
            has_focus: floating_has_focus,
            focus_ring,
            layer,
        } = params;
        let tree = containers;
        let tile_count = containers
            .floating_tiles_in_workspace(self.workspace)
            .count();
        let estimated_capacity = tile_count * 4 + self.closing_windows.len() + tile_count / 2;
        let mut elements = Vec::with_capacity(estimated_capacity);
        let scale = Scale::from(containers.scale(self.workspace));

        // Closing snapshots intentionally stay above live windows. They no longer participate in
        // the tree, and keeping a second, stale stacking model just for their animation would make
        // rendering disagree with the authoritative floating-root order.
        for closing in self
            .closing_windows
            .iter()
            .rev()
            .filter(|_| layer.is_normal() && roots != FloatingRoots::Sticky)
        {
            let elem = closing.render(ctx.as_gles(), view_rect, scale);
            elements.push(elem.into());
        }

        let active = containers.active_floating_window_id(self.workspace);
        let fullscreen_key = containers.floating_fullscreen_key(self.workspace);
        let fullscreen_id = containers
            .floating_fullscreen_window_id(self.workspace)
            .cloned();
        let selection_is_container = self
            .active_container_idx(containers)
            .is_some_and(|idx| self.selected_is_container_in(containers, idx));
        let selection_in_pass = self
            .active_container_idx(containers)
            .is_some_and(|idx| roots.includes(containers, self.root(containers, idx)));

        // Like tiling, push container selection before the regular window
        // contents so it stays visually on top after the global reverse-order
        // composition pass in the renderer.
        if (focus_ring || floating_has_focus)
            && selection_is_container
            && selection_in_pass
            && fullscreen_id.is_none()
        {
            if let Some(idx) = self.active_container_idx(containers) {
                if let Some((_, local_rect, _)) = self
                    .selected_key_in(containers, idx)
                    .and_then(|key| tree.container_info(key))
                {
                    let rect = local_rect;
                    render_container_selection(
                        ctx.renderer,
                        rect,
                        view_rect,
                        containers.scale(self.workspace),
                        floating_has_focus,
                        containers.options(self.workspace).layout.focus_ring,
                        containers.options(self.workspace).layout.border,
                        &mut |elem| elements.push(FloatingRenderElement::ContainerSelection(elem)),
                    );
                }
            }
        }

        if !containers.options(self.workspace).layout.tab_bar.off && fullscreen_id.is_none() {
            let tab_bar_config = containers.options(self.workspace).layout.tab_bar.clone();
            let gles = ctx.renderer.as_gles_renderer();
            let pass_roots: Vec<NodeKey> = if let Some(scope) = fullscreen_key {
                vec![scope]
            } else {
                tree.floating_roots(self.workspace)
                    .filter(|root| roots.includes(containers, *root))
                    .collect()
            };
            for root in pass_roots {
                let gap = containers.branch_gap(root);
                for info in tree.tab_bar_layouts_in_branch(root) {
                    let mut info = info.clone();
                    if gap > 0.0 && info.key == root {
                        info.rect.loc.x -= gap;
                        info.rect.loc.y -= gap;
                        info.rect.size.w = (info.rect.size.w + gap * 2.0).max(0.0);
                    }
                    if let Some(elem) = containers.tab_bar_element(
                        gles,
                        ctx.target,
                        &info,
                        &tab_bar_config,
                        floating_has_focus,
                        containers.scale(self.workspace),
                    ) {
                        elements.push(FloatingRenderElement::TabBar(elem));
                    }
                }
            }
        }

        if let Some(fullscreen_id) = fullscreen_id.as_ref() {
            // Only render the fullscreen tile at (0, 0).
            if let Some(tile) = containers
                .floating_tiles_in_workspace(self.workspace)
                .find(|t| t.window().id() == fullscreen_id)
                .filter(|_| fullscreen_key.is_some_and(|key| roots.includes_node(containers, key)))
            {
                let is_focused = floating_has_focus;
                let pos = Point::from((0.0, 0.0));
                let tile_xray_pos = xray_pos.offset(pos);
                tile.render(
                    ctx.r(),
                    pos,
                    tile_xray_pos,
                    focus_ring && is_focused,
                    &mut |elem| elements.push(elem.into()),
                );
            }
        } else {
            for (key, tile, tile_pos) in
                containers.floating_tiles_with_render_positions_keyed(self.workspace)
            {
                // Skip tiles belonging to a different render layer.
                if layer.is_normal() == tile.is_moving_between_workspaces() {
                    continue;
                }
                if fullscreen_key.is_some_and(|scope| !tree.is_descendant(key, scope)) {
                    continue;
                }
                if !roots.includes_node(containers, key) {
                    continue;
                }
                // Skip tiles entirely outside the viewport (culling)
                let tile_rect = Rectangle::new(tile_pos, tile.tile_size());
                if !tile_rect.overlaps(view_rect) {
                    continue;
                }

                let is_focused = floating_has_focus
                    && Some(tile.window().id()) == active.as_ref()
                    && !selection_is_container;
                let draw_focus = focus_ring && is_focused;
                let tile_xray_pos = xray_pos.offset(tile_pos);

                tile.render(ctx.r(), tile_pos, tile_xray_pos, draw_focus, &mut |elem| {
                    elements.push(elem.into())
                });
            }
        }

        elements
    }

    #[allow(clippy::too_many_arguments)]
    pub(super) fn render<R: NiriRenderer>(
        &self,
        containers: &ContainerTree<W>,
        ctx: RenderCtx<R>,
        xray_pos: XrayPos,
        view_rect: Rectangle<f64, Logical>,
        params: BranchRenderParams,
        roots: FloatingRoots,
        push: &mut dyn FnMut(FloatingRenderElement<R>),
    ) {
        for elem in self.render_elements(containers, ctx, xray_pos, view_rect, params, roots) {
            push(elem);
        }
    }

    pub fn interactive_resize_begin(
        &mut self,
        containers: &ContainerTree<W>,
        window: W::Id,
        edges: ResizeEdge,
    ) -> bool {
        let tree = containers;
        if self.interactive_resize.is_some() {
            return false;
        }

        let Some(idx) = self.idx_of(containers, &window) else {
            return false;
        };

        let root = self.root(containers, idx);
        let Some(key) = tree.window_key(&window) else {
            return false;
        };
        let Some(tile) = tree.get_tile(key) else {
            return false;
        };

        let original_window_size = tile.window_size();
        let container_area = tree
            .floating_container_area(root)
            .expect("every floating stack entry must name a floating root");
        let original_window_pos = container_area.loc;
        let original_container_size = container_area.size;
        let resize_container_edges =
            super::container_tree::branch_display_layouts(containers, root)
                .find(|info| info.key == key)
                .map(|info| {
                    let mut rect = info.rect;
                    rect.loc -= container_area.loc;
                    Self::external_edges_for_rect(container_area.size, rect, edges)
                })
                .unwrap_or(ResizeEdge::empty());

        let resize = InteractiveResize {
            window,
            original_window_size,
            original_window_pos: Some(original_window_pos),
            original_container_size,
            resize_container_edges,
            data: InteractiveResizeData { edges },
        };
        self.interactive_resize = Some(resize);

        true
    }

    pub fn interactive_resize_update(
        &mut self,
        containers: &mut ContainerTree<W>,
        window: &W::Id,
        delta: Point<f64, Logical>,
    ) -> bool {
        let Some(idx) = self.idx_of(containers, window) else {
            return false;
        };

        let (
            original_window_size,
            original_container_size,
            edges,
            original_pos,
            resize_container_edges,
        ) = {
            let Some(resize) = &self.interactive_resize else {
                return false;
            };
            if window != &resize.window {
                return false;
            }
            (
                resize.original_window_size,
                resize.original_container_size,
                resize.data.edges,
                resize.original_window_pos,
                resize.resize_container_edges,
            )
        };
        let (mut min_size, mut max_size, resize_container_h, resize_container_v) = {
            let Some(tile) = containers
                .window_key(window)
                .and_then(|key| containers.get_tile(key))
            else {
                return false;
            };
            let resize_container_h = resize_container_edges.intersects(ResizeEdge::LEFT_RIGHT);
            let resize_container_v = resize_container_edges.intersects(ResizeEdge::TOP_BOTTOM);
            (
                tile.window().min_size(),
                tile.window().max_size(),
                resize_container_h,
                resize_container_v,
            )
        };
        if resize_container_h {
            min_size.w = 0;
            max_size.w = 0;
        }
        if resize_container_v {
            min_size.h = 0;
            max_size.h = 0;
        }

        let mut mouse_move_x = delta.x;
        let mut mouse_move_y = delta.y;
        if edges == ResizeEdge::TOP || edges == ResizeEdge::BOTTOM {
            mouse_move_x = 0.0;
        }
        if edges == ResizeEdge::LEFT || edges == ResizeEdge::RIGHT {
            mouse_move_y = 0.0;
        }

        let grow_width = if edges.contains(ResizeEdge::LEFT) {
            -mouse_move_x
        } else {
            mouse_move_x
        };
        let grow_height = if edges.contains(ResizeEdge::TOP) {
            -mouse_move_y
        } else {
            mouse_move_y
        };

        let base_width = if resize_container_h {
            original_container_size.w
        } else {
            original_window_size.w
        };
        let base_height = if resize_container_v {
            original_container_size.h
        } else {
            original_window_size.h
        };

        let mut target_width = (base_width + grow_width).round() as i32;
        let mut target_height = (base_height + grow_height).round() as i32;
        target_width = ensure_min_max_size_maybe_zero(target_width, min_size.w, max_size.w);
        target_height = ensure_min_max_size_maybe_zero(target_height, min_size.h, max_size.h);
        let effective_grow_width = f64::from(target_width) - base_width;
        let effective_grow_height = f64::from(target_height) - base_height;

        if edges.intersects(ResizeEdge::LEFT_RIGHT) {
            if resize_container_h {
                self.resize_container_dimension(
                    containers,
                    idx,
                    SizeChange::SetFixed(target_width),
                    ResizeAxis::Horizontal,
                    FloatingResizeAnchor::KeepOrigin,
                );
            } else {
                self.set_window_width(
                    containers,
                    Some(window),
                    SizeChange::SetFixed(target_width),
                    false,
                );
            }
        }

        if edges.intersects(ResizeEdge::TOP_BOTTOM) {
            if resize_container_v {
                self.resize_container_dimension(
                    containers,
                    idx,
                    SizeChange::SetFixed(target_height),
                    ResizeAxis::Vertical,
                    FloatingResizeAnchor::KeepOrigin,
                );
            } else {
                self.set_window_height(
                    containers,
                    Some(window),
                    SizeChange::SetFixed(target_height),
                    false,
                );
            }
        }

        if let Some(original_pos) = original_pos {
            let mut move_pos = Point::from((0., 0.));
            if resize_container_h {
                if edges.contains(ResizeEdge::LEFT) {
                    move_pos.x = -effective_grow_width;
                } else if edges.contains(ResizeEdge::RIGHT) {
                    move_pos.x = 0.0;
                } else {
                    move_pos.x = -effective_grow_width / 2.0;
                }
            }
            if resize_container_v {
                if edges.contains(ResizeEdge::TOP) {
                    move_pos.y = -effective_grow_height;
                } else if edges.contains(ResizeEdge::BOTTOM) {
                    move_pos.y = 0.0;
                } else {
                    move_pos.y = -effective_grow_height / 2.0;
                }
            }
            if (resize_container_h && move_pos.x != 0.0)
                || (resize_container_v && move_pos.y != 0.0)
            {
                self.set_container_logical_pos(containers, idx, original_pos + move_pos);
                let root = self.root(containers, idx);
                containers.layout_branch(root);
            }
        }

        true
    }

    /// Consume a tree effect that may have moved the resize target to the tiled branch.
    pub(super) fn cancel_resize_if_target_left(&mut self, containers: &ContainerTree<W>) {
        if self
            .interactive_resize
            .as_ref()
            .is_some_and(|resize| !self.contains(containers, &resize.window))
        {
            self.interactive_resize = None;
        }
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

    pub fn refresh(
        &mut self,
        containers: &mut ContainerTree<W>,
        is_active: bool,
        is_focused: bool,
    ) {
        let _span = tracy_client::span!("FloatingRuntime::refresh");
        let active = containers.active_floating_window_id(self.workspace);
        let deactivate_unfocused = containers
            .options(self.workspace)
            .deactivate_unfocused_windows;
        let disable_resize_throttling =
            containers.options(self.workspace).disable_resize_throttling;
        let border_base = containers.options(self.workspace).layout.border;
        let working_area_size = containers.working_area(self.workspace).size;
        let resize_target = self.interactive_resize.as_ref().and_then(|resize| {
            let idx = self.idx_of(containers, &resize.window)?;
            let mut ids = Vec::new();
            let root = self.root(containers, idx);
            for tile in containers.tiles_in_branch(root) {
                ids.push(tile.window().id().clone());
            }
            Some((resize.data, ids))
        });
        for tile in containers.floating_tiles_mut_in_workspace(self.workspace) {
            let resize_data = resize_target.as_ref().and_then(|(data, ids)| {
                ids.iter()
                    .any(|id| id == tile.window().id())
                    .then_some(*data)
            });

            let win = tile.window_mut();
            win.set_active_in_column(true);
            win.set_floating(true);

            let mut is_active = is_active && Some(win.id()) == active.as_ref();
            if deactivate_unfocused {
                is_active &= is_focused;
            }
            win.set_activated(is_active);

            win.set_interactive_resize(resize_data);

            let border_config = border_base.merged_with(&win.rules().border);
            let bounds = compute_toplevel_bounds(border_config, working_area_size);
            win.set_bounds(bounds);

            // If transactions are disabled, also disable combined throttling, for more
            // intuitive behavior.
            let intent = if disable_resize_throttling {
                ConfigureIntent::CanSend
            } else {
                win.configure_intent()
            };

            if matches!(
                intent,
                ConfigureIntent::CanSend | ConfigureIntent::ShouldSend
            ) {
                win.send_pending_configure();
            }

            win.refresh();
        }
    }

    pub fn clamp_within_working_area(
        &self,
        working_area: Rectangle<f64, Logical>,
        pos: Point<f64, Logical>,
        size: Size<f64, Logical>,
    ) -> Point<f64, Logical> {
        let mut rect = Rectangle::new(pos, size);
        clamp_preferring_top_left_in_area(working_area, &mut rect);
        rect.loc
    }

    pub fn scale_by_working_area(
        &self,
        working_area: Rectangle<f64, Logical>,
        pos: Point<f64, SizeFrac>,
    ) -> Point<f64, Logical> {
        scale_floating_position(working_area, pos)
    }

    pub fn logical_to_size_frac(
        &self,
        working_area: Rectangle<f64, Logical>,
        logical_pos: Point<f64, Logical>,
    ) -> Point<f64, SizeFrac> {
        floating_position_from_logical(working_area, logical_pos)
    }

    fn move_container_and_animate(
        &mut self,
        containers: &mut ContainerTree<W>,
        idx: usize,
        new_pos: Point<f64, Logical>,
    ) {
        // Moves up to this logical pixel distance are not animated.
        const ANIMATION_THRESHOLD_SQ: f64 = 10. * 10.;

        let prev_pos = self.container_area(containers, idx).loc;
        let new_pos = self.set_container_logical_pos(containers, idx, new_pos);

        let diff = prev_pos - new_pos;
        if diff.x * diff.x + diff.y * diff.y > ANIMATION_THRESHOLD_SQ {
            let delta = prev_pos - new_pos;
            let root = self.root(containers, idx);
            for tile in containers.tiles_in_branch_mut(root) {
                tile.animate_move_from(delta);
            }
        }
    }

    pub fn new_window_size(
        &self,
        containers: &ContainerTree<W>,
        width: Option<PresetSize>,
        height: Option<PresetSize>,
        rules: &ResolvedWindowRules,
    ) -> Size<i32, Logical> {
        let border = containers
            .options(self.workspace)
            .layout
            .border
            .merged_with(&rules.border);

        let resolve = |size: Option<PresetSize>, working_area_size: f64| {
            if let Some(size) = size {
                let size = match resolve_preset_size(size, working_area_size) {
                    ResolvedSize::Tile(mut size) => {
                        if !border.off {
                            size -= border.width * 2.;
                        }
                        size
                    }
                    ResolvedSize::Window(size) => size,
                };

                max(1, size.floor() as i32)
            } else {
                0
            }
        };

        let width = resolve(width, containers.working_area(self.workspace).size.w);
        let height = resolve(height, containers.working_area(self.workspace).size.h);

        Size::from((width, height))
    }

    pub fn stored_or_default_tile_pos(
        &self,
        working_area: Rectangle<f64, Logical>,
        tile: &Tile<W>,
    ) -> Option<Point<f64, Logical>> {
        if tile.is_scratchpad() && tile.floating_pos.is_none() {
            return None;
        }

        let pos = tile
            .floating_pos
            .map(|pos| self.scale_by_working_area(working_area, pos));
        pos.or_else(|| {
            tile.window().rules().default_floating_position.map(|pos| {
                let relative_to = pos.relative_to;
                let size = tile.tile_size();
                let area = working_area;

                let mut pos = Point::from((pos.x.0, pos.y.0));
                if relative_to == RelativeTo::TopRight
                    || relative_to == RelativeTo::BottomRight
                    || relative_to == RelativeTo::Right
                {
                    pos.x = area.size.w - size.w - pos.x;
                }
                if relative_to == RelativeTo::BottomLeft
                    || relative_to == RelativeTo::BottomRight
                    || relative_to == RelativeTo::Bottom
                {
                    pos.y = area.size.h - size.h - pos.y;
                }
                if relative_to == RelativeTo::Top || relative_to == RelativeTo::Bottom {
                    pos.x += area.size.w / 2.0 - size.w / 2.0
                }
                if relative_to == RelativeTo::Left || relative_to == RelativeTo::Right {
                    pos.y += area.size.h / 2.0 - size.h / 2.0
                }

                pos + working_area.loc
            })
        })
    }

    pub(super) fn verify_runtime_invariants(&self, containers: &ContainerTree<W>) {
        if let Some(resize) = &self.interactive_resize {
            assert!(
                self.contains(containers, &resize.window),
                "interactive resize window must be present in tiles"
            );
        }
    }
}

pub(super) fn compute_toplevel_bounds(
    border_config: tiri_config::Border,
    working_area_size: Size<f64, Logical>,
) -> Size<i32, Logical> {
    let mut border = 0.;
    if !border_config.off {
        border = border_config.width * 2.;
    }

    Size::from((
        f64::max(working_area_size.w - border, 1.),
        f64::max(working_area_size.h - border, 1.),
    ))
    .to_i32_floor()
}

fn resolve_preset_size(preset: PresetSize, view_size: f64) -> ResolvedSize {
    match preset {
        PresetSize::Proportion(proportion) => ResolvedSize::Tile(view_size * proportion),
        PresetSize::Fixed(width) => ResolvedSize::Window(f64::from(width)),
    }
}
