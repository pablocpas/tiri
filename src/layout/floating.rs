use std::cmp::max;
use std::rc::Rc;

use log::warn;
use smithay::backend::renderer::element::Kind;
use smithay::backend::renderer::gles::GlesRenderer;
use smithay::utils::{Logical, Point, Rectangle, Scale, Serial, Size};
use tiri_config::utils::MergeWith as _;
use tiri_config::{PresetSize, RelativeTo};
use tiri_ipc::{
    ColumnDisplay, LayoutTreeFloatingRootKind, LayoutTreeNode, PositionChange, SizeChange,
    WindowLayout,
};

use super::closing_window::{ClosingWindow, ClosingWindowRenderElement};
use super::container::{
    floating_position_from_logical, scale_floating_position, Direction, FloatingRootKind,
    InactiveTilingReference, InsertParentInfo, Layout, NodeKey, TabBarInfo,
};
use super::container_tree::{percent_from_size_change, ContainerTree, LeafFrameInfo, TileConfig};
use super::focus_ring::{render_container_selection, FocusRingEdges, FocusRingRenderElement};
use super::tile::{Tile, TileRenderElement, TileRenderSnapshot};
use super::workspace::{InteractiveResize, ResolvedSize};
use super::{
    resize_edges_for_point, ConfigureIntent, InteractiveResizeData, LayoutCycleEntry,
    LayoutElement, Options, RemovedTile, ResizeAxis, ResizeRequest, SizeFrac,
};
use crate::animation::Animation;
use crate::layout::tab_bar::{
    render_tab_bar, tab_bar_state_from_info, TabBarCacheEntry, TabBarRenderOutput,
};
use crate::layout::RenderLayer;
use crate::niri_render_elements;
use crate::render_helpers::primary_gpu_texture::PrimaryGpuTextureRenderElement;
use crate::render_helpers::renderer::NiriRenderer;
use crate::render_helpers::texture::TextureRenderElement;
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

/// The floating side of a workspace: sway's `ws->floating`, and what tiri hangs off it.
///
/// The groups, their geometry and their stacking order live in one authoritative collection
/// inside the workspace's container tree. This side owns only transient interaction/render
/// state. A container crossing between lists is a move, not a reconstruction, so its key and
/// its place in the seat's order survive the crossing the way they do in sway.
#[derive(Debug)]
pub struct FloatingSpace<W: LayoutElement> {
    /// Ongoing interactive resize.
    interactive_resize: Option<InteractiveResize<W>>,

    /// Windows in the closing animation.
    closing_windows: Vec<ClosingWindow>,

    /// Copy-only projection reused while rendering floating branches.
    render_layout_scratch: Vec<LeafFrameInfo>,
}

niri_render_elements! {
    FloatingSpaceRenderElement<R> => {
        Tile = TileRenderElement<R>,
        TabBar = PrimaryGpuTextureRenderElement,
        ClosingWindow = ClosingWindowRenderElement,
        ContainerSelection = FocusRingRenderElement,
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum FloatingResizeAnchor {
    Center,
    KeepOrigin,
}

impl FloatingRootKind {
    fn ipc(self) -> LayoutTreeFloatingRootKind {
        match self {
            Self::ImplicitWindowGroup => LayoutTreeFloatingRootKind::ImplicitWindowGroup,
            Self::FloatedContainer => LayoutTreeFloatingRootKind::FloatedContainer,
            Self::WorkspaceWrapper => LayoutTreeFloatingRootKind::WorkspaceWrapper,
        }
    }
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

/// All tiles across the floating containers, in container order.
fn floating_tile_iter<'a, W: LayoutElement>(
    containers: &'a ContainerTree<W>,
) -> impl Iterator<Item = &'a Tile<W>> + 'a {
    let tree = containers.arena();
    tree.floating_roots()
        .flat_map(|root| tree.tiles_in_branch(root))
}

/// All tiles across the floating containers (mutable), in container order.
fn floating_tile_iter_mut<'a, W: LayoutElement>(
    containers: &'a mut ContainerTree<W>,
) -> impl Iterator<Item = &'a mut Tile<W>> + 'a {
    let keys: Vec<NodeKey> = containers
        .arena()
        .floating_roots()
        .flat_map(|root| containers.arena().leaf_keys_in_branch(root))
        .collect();
    containers
        .arena_mut()
        .tiles_mut_for_keys(&keys)
        .into_iter()
        .map(|(_, tile)| tile)
}

impl<W: LayoutElement> FloatingSpace<W> {
    fn leaf_point_hits_tab_bar(
        tab_bar_infos: &[TabBarInfo],
        leaf_path: &[usize],
        pos_in_container: Point<f64, Logical>,
        gap: f64,
        scale: f64,
    ) -> bool {
        let eps = 1.0 / scale.max(1e-6);
        tab_bar_infos.iter().any(|info| {
            if !leaf_path.starts_with(info.path.as_slice()) {
                return false;
            }

            let mut rect = info.rect;
            if gap > 0.0 && info.path.is_empty() {
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

    fn leaf_has_tab_bar_ancestor(tab_bar_infos: &[TabBarInfo], leaf_path: &[usize]) -> bool {
        tab_bar_infos
            .iter()
            .any(|info| leaf_path.starts_with(info.path.as_slice()))
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

    pub fn new() -> Self {
        Self {
            interactive_resize: None,
            closing_windows: Vec::new(),
            render_layout_scratch: Vec::new(),
        }
    }

    fn root(containers: &ContainerTree<W>, idx: usize) -> NodeKey {
        containers
            .arena()
            .floating_root_at(idx)
            .expect("floating index must name an authoritative root entry")
    }

    fn root_count(containers: &ContainerTree<W>) -> usize {
        containers.arena().floating_root_count()
    }

    fn container_area(&self, containers: &ContainerTree<W>, idx: usize) -> Rectangle<f64, Logical> {
        let root = Self::root(containers, idx);
        containers
            .arena()
            .floating_container_area(root)
            .expect("every floating stack entry must name a floating root")
    }

    fn set_container_logical_pos(
        &mut self,
        containers: &mut ContainerTree<W>,
        idx: usize,
        logical_pos: Point<f64, Logical>,
    ) -> Point<f64, Logical> {
        let root = Self::root(containers, idx);
        containers
            .arena_mut()
            .set_floating_logical_pos(root, logical_pos)
    }

    fn set_container_size(
        &mut self,
        containers: &mut ContainerTree<W>,
        idx: usize,
        size: Size<f64, Logical>,
    ) -> Rectangle<f64, Logical> {
        let root = Self::root(containers, idx);
        containers.arena_mut().set_floating_size(root, size)
    }

    pub fn update_config(
        &mut self,
        containers: &mut ContainerTree<W>,
        view_size: Size<f64, Logical>,
        working_area: Rectangle<f64, Logical>,
        scale: f64,
        options: Rc<Options>,
    ) {
        let roots: Vec<_> = containers.arena().floating_roots().collect();
        let tree = containers.arena_mut();
        for root in roots {
            tree.update_floating_working_area(root, working_area);
        }
        tree.layout();

        for tile in self.tiles_mut(containers) {
            tile.update_config(view_size, scale, options.clone());
        }
    }

    pub fn update_shaders(&mut self, containers: &mut ContainerTree<W>) {
        for tile in self.tiles_mut(containers) {
            tile.update_shaders();
        }
    }

    pub fn advance_animations(&mut self, containers: &mut ContainerTree<W>) {
        for tile in self.tiles_mut(containers) {
            tile.advance_animations();
        }

        self.closing_windows.retain_mut(|closing| {
            closing.advance_animations();
            closing.are_animations_ongoing()
        });
    }

    pub fn are_animations_ongoing(&self, containers: &ContainerTree<W>) -> bool {
        self.tiles(containers).any(Tile::are_animations_ongoing) || !self.closing_windows.is_empty()
    }

    pub fn are_transitions_ongoing(&self, containers: &ContainerTree<W>) -> bool {
        self.tiles(containers).any(Tile::are_transitions_ongoing)
            || !self.closing_windows.is_empty()
    }

    pub fn update_render_elements(
        &mut self,
        containers: &mut ContainerTree<W>,
        is_active: bool,
        view_rect: Rectangle<f64, Logical>,
        layer: RenderLayer,
    ) {
        let _span = tracy_client::span!("FloatingSpace::update_render_elements");
        let active = self.active_window_id(containers);
        let fullscreen_id = self.fullscreen_window_id(containers).cloned();
        let selection_is_container = self
            .active_container_idx(containers)
            .is_some_and(|idx| self.selected_is_container_in(containers, idx));
        let scale = containers.scale();
        let floating_has_focus = containers.side_is_active(true);
        let applied = containers.arena_mut().apply_pending_layouts_if_ready();
        if applied && containers.arena_mut().take_pending_relayout() {
            containers.arena_mut().layout();
        }
        let mut layouts = std::mem::take(&mut self.render_layout_scratch);
        let roots: Vec<_> = containers.arena().floating_roots().collect();
        for root in roots {
            layouts.clear();
            {
                let _span =
                    tracy_client::span!("FloatingSpace::project_display_layouts_for_render");
                layouts.extend(
                    super::container_tree::branch_display_layouts(containers.arena(), root)
                        .map(LeafFrameInfo::from),
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
            let float_has_sublayout = containers.arena().window_count_in_branch(root) > 1;
            for info in layouts.iter().copied() {
                let is_focus_head =
                    float_has_sublayout && containers.arena().is_focus_head(info.key);
                // Same rule as tiling: under a tab bar the tab is the top decoration.
                let mut edges = FocusRingEdges::all();
                if containers.arena().parent_is_switcher(info.key) {
                    edges.top = false;
                }
                if let Some(tile) = containers.arena_mut().get_tile_mut(info.key) {
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

    pub fn tiles<'a>(
        &'a self,
        containers: &'a ContainerTree<W>,
    ) -> impl Iterator<Item = &'a Tile<W>> + 'a {
        floating_tile_iter(containers)
    }

    pub fn tiles_mut<'a>(
        &'a mut self,
        containers: &'a mut ContainerTree<W>,
    ) -> impl Iterator<Item = &'a mut Tile<W>> + 'a {
        floating_tile_iter_mut(containers)
    }

    pub fn tiles_with_offsets<'a>(
        &'a self,
        containers: &'a ContainerTree<W>,
    ) -> impl Iterator<Item = (&'a Tile<W>, Point<f64, Logical>)> + 'a {
        let tree = containers.arena();
        let mut tiles = Vec::new();
        for root in tree.floating_roots() {
            for info in super::container_tree::branch_display_layouts(containers.arena(), root) {
                if let Some(tile) = tree.get_tile(info.key) {
                    tiles.push((tile, info.rect.loc));
                }
            }
        }
        tiles.into_iter()
    }

    pub(super) fn resize_hit_under(
        &self,
        containers: &ContainerTree<W>,
        pos: Point<f64, Logical>,
    ) -> FloatingResizeResult<W::Id> {
        let tree = containers.arena();
        if self.has_fullscreen_window(containers) {
            return FloatingResizeResult::None;
        }

        let scale = Scale::from(containers.scale());
        for root in tree.floating_roots() {
            let container_area = tree
                .floating_container_area(root)
                .expect("every floating stack entry must name a floating root");
            let gap = containers.branch_gap(root);
            let tab_bar_infos = tree.tab_bar_layouts_in_branch(root);
            for info in super::container_tree::branch_display_layouts(containers.arena(), root)
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
                    &tab_bar_infos,
                    &info.path,
                    pos,
                    gap,
                    containers.scale(),
                ) {
                    return FloatingResizeResult::Blocked;
                }

                let pos_within_tile = pos - tile_pos;
                let size = tile.tile_size();
                let mut edges =
                    resize_edges_for_point(pos_within_tile, size, tile.effective_border_width());
                if Self::leaf_has_tab_bar_ancestor(&tab_bar_infos, &info.path) {
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

    pub fn resize_edges_under(
        &self,
        containers: &ContainerTree<W>,
        pos: Point<f64, Logical>,
    ) -> Option<ResizeEdge> {
        match self.resize_hit_under(containers, pos) {
            FloatingResizeResult::Hit(hit) => Some(hit.edges),
            FloatingResizeResult::Blocked => Some(ResizeEdge::empty()),
            FloatingResizeResult::None => None,
        }
    }

    fn tiles_with_offsets_visible<'a>(
        &'a self,
        containers: &'a ContainerTree<W>,
    ) -> impl Iterator<Item = (&'a Tile<W>, Point<f64, Logical>)> + 'a {
        let tree = containers.arena();
        let mut tiles = Vec::new();
        for root in tree.floating_roots() {
            for info in super::container_tree::branch_display_layouts(containers.arena(), root)
                .filter(|info| info.visible)
            {
                if let Some(tile) = tree.get_tile(info.key) {
                    tiles.push((tile, info.rect.loc));
                }
            }
        }
        tiles.into_iter()
    }

    pub fn tiles_with_offsets_mut<'a>(
        &'a mut self,
        containers: &'a mut ContainerTree<W>,
    ) -> impl Iterator<Item = (&'a mut Tile<W>, Point<f64, Logical>)> + 'a {
        let mut keys = Vec::new();
        let mut locs = Vec::new();
        let roots: Vec<_> = containers.arena().floating_roots().collect();
        for root in roots {
            for info in super::container_tree::branch_display_layouts(containers.arena(), root) {
                keys.push(info.key);
                locs.push(info.rect.loc);
            }
        }
        containers
            .arena_mut()
            .tiles_mut_for_keys(&keys)
            .into_iter()
            .map(move |(idx, tile)| (tile, locs[idx]))
    }

    pub fn tiles_with_render_positions<'a>(
        &'a self,
        containers: &'a ContainerTree<W>,
    ) -> impl Iterator<Item = (&'a Tile<W>, Point<f64, Logical>)> + 'a {
        let scale = containers.scale();
        self.tiles_with_offsets_visible(containers)
            .map(move |(tile, offset)| {
                let pos = offset + tile.render_offset();
                // Round to physical pixels.
                let pos = pos.to_physical_precise_round(scale).to_logical(scale);
                (tile, pos)
            })
    }

    fn tab_bar_hit<'a>(
        &'a self,
        containers: &'a ContainerTree<W>,
        pos: Point<f64, Logical>,
    ) -> Option<(&'a W, super::HitType)> {
        // A 1px pad makes the floating bar's edges forgiving to hit: next to them is the
        // desktop, not another window.
        containers
            .arena()
            .floating_roots()
            .find_map(|root| containers.branch_tab_bar_hit(root, pos, 1))
    }

    pub fn window_under<'a>(
        &'a self,
        containers: &'a ContainerTree<W>,
        pos: Point<f64, Logical>,
    ) -> Option<(&'a W, super::HitType)> {
        if let Some(fullscreen_id) = self.fullscreen_window_id(containers) {
            let tile = self
                .tiles(containers)
                .find(|t| t.window().id() == fullscreen_id)?;
            return super::HitType::hit_tile(tile, Point::from((0.0, 0.0)), pos);
        }

        let fullscreen_scope = self.fullscreen_key(containers);
        let tab_hit = if let Some(scope) = fullscreen_scope {
            containers.branch_tab_bar_hit(scope, pos, 1)
        } else {
            self.tab_bar_hit(containers, pos)
        };
        if let Some(hit) = tab_hit {
            return Some(hit);
        }

        for (tile, tile_pos) in self.tiles_with_render_positions(containers) {
            if fullscreen_scope
                .is_some_and(|scope| !containers.arena().is_descendant(tile.node_key(), scope))
            {
                continue;
            }
            if let Some(rv) = super::HitType::hit_tile(tile, tile_pos, pos) {
                return Some(rv);
            }
        }

        None
    }

    pub fn tiles_with_render_positions_mut<'a>(
        &'a mut self,
        containers: &'a mut ContainerTree<W>,
        round: bool,
    ) -> impl Iterator<Item = (&'a mut Tile<W>, Point<f64, Logical>)> + 'a {
        let scale = containers.scale();
        self.tiles_with_offsets_mut(containers)
            .map(move |(tile, offset)| {
                let mut pos = offset + tile.render_offset();
                // Round to physical pixels.
                if round {
                    pos = pos.to_physical_precise_round(scale).to_logical(scale);
                }
                (tile, pos)
            })
    }

    pub fn tiles_with_ipc_layouts<'a>(
        &'a self,
        containers: &'a ContainerTree<W>,
    ) -> impl Iterator<Item = (&'a Tile<W>, WindowLayout)> + 'a {
        let scale = containers.scale();
        self.tiles_with_offsets(containers)
            .map(move |(tile, offset)| {
                // Do not include animated render offset here to avoid IPC spam.
                let pos = offset;
                // Round to physical pixels.
                let pos = pos.to_physical_precise_round(scale).to_logical(scale);

                let layout = WindowLayout {
                    tile_pos_in_workspace_view: Some(pos.into()),
                    ..tile.ipc_layout_template()
                };
                (tile, layout)
            })
    }

    pub fn new_window_toplevel_bounds(
        &self,
        containers: &ContainerTree<W>,
        rules: &ResolvedWindowRules,
    ) -> Size<i32, Logical> {
        let border_config = containers
            .options()
            .layout
            .border
            .merged_with(&rules.border);
        compute_toplevel_bounds(border_config, containers.working_area().size)
    }

    /// Returns the geometry of the active window relative to and clamped to the working area.
    ///
    /// During animations, assumes the final tile position.
    pub fn active_window_visual_rectangle(
        &self,
        containers: &ContainerTree<W>,
    ) -> Option<Rectangle<f64, Logical>> {
        let active_id = self.active_window_id(containers)?;
        let (tile, offset) = self
            .tiles_with_offsets_visible(containers)
            .find(|(tile, _)| tile.window().id() == &active_id)?;

        let window_pos = offset + tile.window_loc();
        let window_size = tile.window_size();
        let window_rect = Rectangle::new(window_pos, window_size);

        containers.working_area().intersection(window_rect)
    }

    pub fn popup_target_rect(
        &self,
        containers: &ContainerTree<W>,
        id: &W::Id,
    ) -> Option<Rectangle<f64, Logical>> {
        for (tile, pos) in self.tiles_with_offsets_visible(containers) {
            if tile.window().id() == id {
                // Position within the working area.
                let mut target = containers.working_area();
                target.loc -= pos;
                target.loc -= tile.window_loc();

                return Some(target);
            }
        }
        None
    }

    fn idx_of(&self, containers: &ContainerTree<W>, id: &W::Id) -> Option<usize> {
        let tree = containers.arena();
        let key = tree.window_key(id)?;
        let root = tree.branch_root(key);
        tree.floating_root_index(root)
    }

    fn contains(&self, containers: &ContainerTree<W>, id: &W::Id) -> bool {
        self.idx_of(containers, id).is_some()
    }

    /// The focused floating view, or the floating MRU while tiling has keyboard focus.
    ///
    /// Focus belongs to the workspace tree's seat. `FloatingSpace` deliberately owns no
    /// projection of it; the only ordering kept here is visual stacking.
    fn active_window_id(&self, containers: &ContainerTree<W>) -> Option<W::Id> {
        containers.active_floating_window_id()
    }

    /// Floating projection of the workspace's single fullscreen node.
    pub(super) fn fullscreen_key(&self, containers: &ContainerTree<W>) -> Option<NodeKey> {
        let tree = containers.arena();
        let key = tree.fullscreen_key()?;
        (tree.holds_node(key) && tree.is_in_floating_branch(key)).then_some(key)
    }

    /// Floating leaf that owns fullscreen at the client protocol boundary.
    fn fullscreen_window_id<'a>(&self, containers: &'a ContainerTree<W>) -> Option<&'a W::Id> {
        self.fullscreen_key(containers)?;
        containers.arena().fullscreen_leaf_window_id()
    }

    fn active_container_idx(&self, containers: &ContainerTree<W>) -> Option<usize> {
        let active_id = self.active_window_id(containers)?;
        self.idx_of(containers, &active_id)
    }

    fn selected_is_container_in(&self, containers: &ContainerTree<W>, idx: usize) -> bool {
        containers.selected_container_in(Self::root(containers, idx))
    }

    /// The node a command targets inside container `idx`: its root when the whole floating
    /// wrapper is selected, otherwise the tree's own selection.
    fn selected_key_in(&self, containers: &ContainerTree<W>, idx: usize) -> Option<NodeKey> {
        let tree = containers.arena();
        tree.branch_position(Self::root(containers, idx))
    }

    fn tile_at_mut<'a>(
        &self,
        containers: &'a mut ContainerTree<W>,
        id: &W::Id,
    ) -> Option<&'a mut Tile<W>> {
        let tree = containers.arena_mut();
        let key = tree.window_key(id)?;
        tree.is_in_floating_branch(key)
            .then(|| tree.get_tile_mut(key))
            .flatten()
    }

    pub fn active_window<'a>(&self, containers: &'a ContainerTree<W>) -> Option<&'a W> {
        let tree = containers.arena();
        let id = self.active_window_id(containers)?;
        let key = tree.window_key(&id)?;
        tree.is_in_floating_branch(key)
            .then(|| tree.get_tile(key).map(Tile::window))
            .flatten()
    }

    pub fn active_window_mut<'a>(&self, containers: &'a mut ContainerTree<W>) -> Option<&'a mut W> {
        let id = self.active_window_id(containers)?;
        let tree = containers.arena_mut();
        let key = tree.window_key(&id)?;
        tree.is_in_floating_branch(key)
            .then(|| tree.get_tile_mut(key).map(Tile::window_mut))
            .flatten()
    }

    pub fn has_window(&self, containers: &ContainerTree<W>, id: &W::Id) -> bool {
        let tree = containers.arena();
        tree.window_key(id)
            .is_some_and(|key| tree.is_in_floating_branch(key))
    }

    pub fn is_empty(&self, containers: &ContainerTree<W>) -> bool {
        containers.arena().floating_root_count() == 0
    }

    pub fn set_fullscreen(
        &mut self,
        containers: &mut ContainerTree<W>,
        window: &W::Id,
        is_fullscreen: bool,
    ) {
        if is_fullscreen {
            if self.is_fullscreen(containers, window) {
                return;
            }

            if let Some(previous) = self.fullscreen_window_id(containers).cloned() {
                if previous != *window {
                    self.set_fullscreen(containers, &previous, false);
                }
            }

            let Some(key) = containers
                .arena()
                .window_key(window)
                .filter(|key| containers.arena().is_in_floating_branch(*key))
            else {
                return;
            };

            // Store the floating size before going fullscreen.
            if let Some(tile) = self
                .tiles_mut(containers)
                .find(|t| t.window().id() == window)
            {
                Self::store_floating_size_for_restore(tile);
                tile.request_fullscreen(true, None);
            }

            containers.arena_mut().set_fullscreen_key(Some(key));
        } else {
            if !self.is_fullscreen(containers, window) {
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
            if let Some(tile) = self
                .tiles_mut(containers)
                .find(|t| t.window().id() == window)
            {
                let size = tile.floating_window_size.unwrap_or_default();
                tile.window_mut().request_size_once(size, true);
            }

            containers.arena_mut().set_fullscreen_key(None);
        }
        containers.arena_mut().layout();
    }

    pub fn is_fullscreen(&self, containers: &ContainerTree<W>, window: &W::Id) -> bool {
        self.fullscreen_window_id(containers)
            .is_some_and(|id| id == window)
    }

    pub fn has_fullscreen_window(&self, containers: &ContainerTree<W>) -> bool {
        self.fullscreen_key(containers).is_some()
    }

    pub fn selected_is_container(&self, containers: &ContainerTree<W>, id: Option<&W::Id>) -> bool {
        let active = self.active_window_id(containers);
        let Some(id) = id.or(active.as_ref()) else {
            return false;
        };
        let Some(idx) = self.idx_of(containers, id) else {
            return false;
        };
        self.selected_is_container_in(containers, idx)
    }

    #[cfg(test)]
    pub(super) fn active_wrapper_selected(&self, containers: &ContainerTree<W>) -> bool {
        let tree = containers.arena();
        let Some(idx) = self.active_container_idx(containers) else {
            return false;
        };
        tree.selected_container_key() == Some(Self::root(containers, idx))
    }

    pub(super) fn close_window_ids_for_active_selection(
        &self,
        containers: &ContainerTree<W>,
    ) -> Vec<W::Id> {
        let Some(idx) = self.active_container_idx(containers) else {
            return Vec::new();
        };
        containers.close_window_ids_in_branch(Self::root(containers, idx))
    }

    pub fn clear_selection_context(&self, containers: &mut ContainerTree<W>) {
        let tree = containers.arena_mut();
        tree.clear_selection();
    }

    pub fn add_tile(&mut self, containers: &mut ContainerTree<W>, tile: Tile<W>, activate: bool) {
        self.add_tile_at(containers, 0, tile, activate);
    }

    pub fn add_tile_with_restore_hint(
        &mut self,
        containers: &mut ContainerTree<W>,
        mut tile: Tile<W>,
        activate: bool,
    ) {
        let hint = tile.floating_reinsert_hint.take();

        // A tile that was the whole group has no position inside one to be restored to, and
        // the group it was went away with it; it comes back the way it left, as its own. Nor
        // can a remembered position be restored into a group that is now one window: there is
        // no container there to hold two.
        if let Some((container_id, Some(insert_info))) = hint {
            if let Some(idx) = containers
                .arena()
                .floating_root_index_by_id(container_id)
                .filter(|idx| !containers.branch_root_is_lone_window(Self::root(containers, *idx)))
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
                // If the window was fullscreen or maximized without a floating size, ask for (0, 0).
                floating_size.unwrap_or_default()
            } else {
                // If the window wasn't fullscreen without a floating size (e.g. it was tiled before),
                // ask for the current size. If the current size is unknown (the window was only ever
                // fullscreen until now), fall back to (0, 0).
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
        mut tile: Tile<W>,
        activate: bool,
    ) {
        let config = containers.tile_config();
        // A view that maps fullscreen keeps the state on the list it lands in —
        // `container_set_fullscreen` never moves a node between lists, so arriving floating is
        // not a reason to leave fullscreen. Only a view that is not fullscreen gets normalized
        // to a floating size on the way in.
        let maps_fullscreen = tile.window().pending_sizing_mode().is_fullscreen();
        let requested_tile_size = if maps_fullscreen {
            Self::preserve_fullscreen_tile_for_floating(&config, &mut tile, None);
            // The group's box is the one the view would have had unfullscreened.
            // `container_init_floating` computes it from the natural size whatever the
            // fullscreen state, and it is what `fullscreen disable` restores to later.
            tile.floating_window_size.map(|size| {
                Size::from((
                    tile.tile_width_for_window_width(f64::from(size.w)),
                    tile.tile_height_for_window_height(f64::from(size.h)),
                ))
            })
        } else {
            Self::prepare_tile_for_floating(&config, &mut tile).1
        };

        // Make sure the tile isn't inserted below its parent.
        let roots: Vec<_> = containers.arena().floating_roots().take(idx).collect();
        for (i, root) in roots.into_iter().enumerate() {
            if containers
                .arena_mut()
                .windows_in_branch(root)
                .iter()
                .any(|parent| tile.window().is_child_of(parent))
            {
                idx = i;
                break;
            }
        }

        let tile_size = requested_tile_size.unwrap_or_else(|| tile.tile_size());
        let pos = self
            .stored_or_default_tile_pos(containers.working_area(), &tile)
            .unwrap_or_else(|| {
                center_preferring_top_left_in_area(containers.working_area(), tile_size)
            });
        let rect = Rectangle::new(pos, tile_size);

        let (_root, leaf) = containers.arena_mut().float_new_group(tile, rect, idx);
        // `workspace->fullscreen` names the node on whichever list it lives.
        if maps_fullscreen && containers.arena().fullscreen_key().is_none() {
            containers.arena_mut().set_fullscreen_key(Some(leaf));
        }
        if activate || containers.arena().focused_node_key().is_none() {
            containers.arena_mut().focus_node(leaf);
        }
        containers.arena_mut().layout();

        self.bring_up_descendants_of(containers, idx);
    }

    pub(super) fn add_tile_to_active_container(
        &mut self,
        containers: &mut ContainerTree<W>,
        tile: Tile<W>,
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
        tile: Tile<W>,
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
        mut tile: Tile<W>,
        activate: bool,
    ) -> bool {
        let config = containers.tile_config();
        let (win_id, _) = Self::prepare_tile_for_floating(&config, &mut tile);
        let root = Self::root(containers, idx);
        if containers.arena_mut().selected_container_key() == Some(root) {
            let insert_idx = containers.arena_mut().branch_children_len(root);
            containers
                .arena_mut()
                .insert_leaf_into_branch(root, insert_idx, tile, activate);
        } else {
            containers
                .arena_mut()
                .insert_window_into_branch(root, tile, activate);
        }
        // Sway's view-map path arranges the new view's parent, not the workspace. That is
        // observable when this group contains workspace fullscreen: a workspace arrange
        // stops at the fullscreen owner, while `arrange_container(root)` still lays out the
        // complete floating group in its ordinary box.
        containers.arena_mut().layout_container_subtree(root);

        if activate {
            self.activate_window(containers, &win_id);
        }

        true
    }

    fn add_tile_to_container_idx_with_parent_info(
        &mut self,
        containers: &mut ContainerTree<W>,
        idx: usize,
        mut tile: Tile<W>,
        activate: bool,
        info: &InsertParentInfo,
    ) {
        let config = containers.tile_config();
        let (win_id, _) = Self::prepare_tile_for_floating(&config, &mut tile);

        let root = Self::root(containers, idx);
        let _ = containers
            .arena_mut()
            .insert_leaf_with_parent_info(root, info, tile, activate);
        containers.arena_mut().layout_container_subtree(root);

        if activate {
            self.activate_window(containers, &win_id);
        }
    }

    /// Whether the window named here sits in a container a new sibling can join.
    ///
    /// It used to answer about the *focused* container instead, which is the same node only
    /// when the named window happens to be the focused one. A floating window that is its own
    /// root has no container to join, and reading someone else's answer for it sent a new tile
    /// into a group that was not there.
    pub(super) fn container_allows_splits(
        &self,
        containers: &ContainerTree<W>,
        id: &W::Id,
    ) -> bool {
        let tree = containers.arena();
        let Some(key) = tree.window_key(id) else {
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

    pub fn add_tile_above(
        &mut self,
        containers: &mut ContainerTree<W>,
        above: &W::Id,
        mut tile: Tile<W>,
        activate: bool,
    ) {
        let idx = self.idx_of(containers, above).unwrap();

        let above_area = self.container_area(containers, idx);
        let tile_size = tile.tile_size();
        let pos =
            above_area.loc + (above_area.size.to_point() - tile_size.to_point()).downscale(2.);
        let pos = self.clamp_within_working_area(containers.working_area(), pos, tile_size);
        tile.floating_pos = Some(self.logical_to_size_frac(containers.working_area(), pos));

        self.add_tile_at(containers, idx, tile, activate);
    }

    pub(super) fn add_subtree(
        &mut self,
        containers: &mut ContainerTree<W>,
        key: NodeKey,
        mut rect: Rectangle<f64, Logical>,
        workspace_floated: bool,
    ) -> bool {
        let config = containers.tile_config();
        let fullscreen_key = containers.arena().fullscreen_key();
        let fullscreen_restore_size = fullscreen_key
            .filter(|fullscreen| *fullscreen == key)
            .and_then(|fullscreen| containers.arena().fullscreen_restore_geometry(fullscreen))
            .map(|rect| rect.size);
        let mut prepared_leaf = None;
        let mut preserved_fullscreen_leaf = false;
        if containers.arena_mut().is_leaf(key) {
            if let Some(tile) = containers.arena_mut().get_tile_mut(key) {
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
                let working_area = containers.working_area();
                if let Some(tile) = containers.arena().get_tile(key) {
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
        let root = if workspace_floated {
            containers.arena_mut().float_whole_workspace(area, 0)
        } else {
            containers.arena_mut().float_as_group(key, area, 0, kind)
        };
        let Some(root) = root else {
            return false;
        };

        // `container_floating_resize_and_center` writes the new root's pending box
        // immediately, for a view and for a container alike. A different fullscreen branch
        // can make the following workspace arrange skip this floating branch; its descendants
        // then keep their old boxes, but the root itself already reports this one.
        containers.arena_mut().set_node_geometry(root, area);

        let keys = containers.arena_mut().leaf_keys_in_branch(root);
        for key in keys {
            if prepared_leaf.is_some_and(|(prepared, _)| prepared == key) {
                continue;
            }
            let fullscreen_restore_size = (fullscreen_key == Some(key))
                .then(|| containers.arena().fullscreen_restore_geometry(key))
                .flatten()
                .map(|rect| rect.size);
            if let Some(tile) = containers.arena_mut().get_tile_mut(key) {
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
        containers.arena_mut().layout();

        self.bring_up_descendants_of(containers, 0);
        true
    }

    fn bring_up_descendants_of(&mut self, containers: &mut ContainerTree<W>, idx: usize) {
        let tree = containers.arena();
        let roots: Vec<_> = tree.floating_roots().collect();
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

    pub fn remove_active_tile(
        &mut self,
        containers: &mut ContainerTree<W>,
    ) -> Option<RemovedTile<W>> {
        let id = self.active_window_id(containers)?;
        Some(self.remove_tile(containers, &id))
    }

    pub fn remove_tile(&mut self, containers: &mut ContainerTree<W>, id: &W::Id) -> RemovedTile<W> {
        let idx = self.idx_of(containers, id).unwrap();
        self.remove_tile_from_container(containers, idx, id)
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
        let root = Self::root(containers, idx);

        if let Some(resize) = &self.interactive_resize {
            if self.idx_of(containers, &resize.window) == Some(idx) {
                self.interactive_resize = None;
            }
        }

        for tile in containers.arena_mut().tiles_in_branch_mut(root) {
            Self::store_floating_size_for_restore(tile);
        }
        let changed = if as_workspace {
            containers.arena_mut().unfloat_as_workspace(root)
        } else if let Some(reference) = reference {
            containers
                .arena_mut()
                .unfloat_with_tiling_reference(root, reference)
        } else {
            containers.arena_mut().unfloat_into_workspace(root)
        };
        if !changed {
            return false;
        }
        containers.arena_mut().layout();

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
        let Some(key) = containers.arena_mut().window_key(id) else {
            return false;
        };
        let pos = containers
            .arena()
            .floating_position(Self::root(containers, idx))
            .expect("every floating stack entry must name a floating root");
        if let Some(tile) = containers.arena_mut().get_tile_mut(key) {
            Self::store_floating_size_for_restore(tile);
            tile.floating_pos = Some(pos);
            tile.set_scratchpad(false);
        }
        let Some(group_empty) = containers.arena_mut().unfloat_node(key, reference) else {
            return false;
        };
        let _ = group_empty;
        containers.arena_mut().layout();
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
        self.active_window_id(containers)
            .as_ref()
            .and_then(|id| self.idx_of(containers, id))
            .is_some_and(|idx| containers.arena().floating_root_is_workspace_wrapper(idx))
    }

    fn remove_tile_from_container(
        &mut self,
        containers: &mut ContainerTree<W>,
        idx: usize,
        id: &W::Id,
    ) -> RemovedTile<W> {
        let root = Self::root(containers, idx);
        let container_pos = containers
            .arena()
            .floating_position(root)
            .expect("every floating stack entry must name a floating root");
        let container_id = containers
            .arena()
            .floating_root_id_at(idx)
            .expect("floating index must have a stable reinsertion id");
        let insert_hint = containers.arena().insert_parent_info_for_window(id);
        let mut tile = containers
            .arena_mut()
            .remove_window(id)
            .expect("window must exist in floating container");

        // Stop interactive resize.
        if let Some(resize) = &self.interactive_resize {
            if tile.window().id() == &resize.window {
                self.interactive_resize = None;
            }
        }

        if containers.arena().window_count_in_branch(root) == 0 {
            containers.arena_mut().forget_floating_root(root);
        }

        Self::store_floating_size_for_restore(&mut tile);
        // Store the floating position.
        tile.floating_pos = Some(container_pos);
        tile.floating_reinsert_hint = Some((container_id, insert_hint));

        RemovedTile {
            tile,
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
        let (tile, tile_pos) = self
            .tiles_with_render_positions_mut(containers, false)
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

        containers.arena_mut().clear_selection();
        let _ = containers.arena_mut().focus_window_by_id(id);
        true
    }

    pub fn activate_window(&mut self, containers: &mut ContainerTree<W>, id: &W::Id) -> bool {
        let Some(idx) = self.idx_of(containers, id) else {
            return false;
        };

        self.raise_container(containers, idx, 0);
        self.bring_up_descendants_of(containers, 0);
        containers.arena_mut().clear_selection();
        let _ = containers.arena_mut().focus_window_by_id(id);

        true
    }

    fn raise_container(
        &mut self,
        containers: &mut ContainerTree<W>,
        from_idx: usize,
        to_idx: usize,
    ) {
        assert!(to_idx <= from_idx);
        assert!(containers.arena_mut().move_floating_root(from_idx, to_idx));
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
            containers.options().animations.window_close.anim,
        );

        let scale = Scale::from(containers.scale());
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
        id.cloned().or_else(|| self.active_window_id(containers))
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
        let available_size = containers.working_area().size.w;
        let presets = containers.options().layout.preset_column_widths.clone();

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
        let available_size = containers.working_area().size.h;
        let presets = containers.options().layout.preset_window_heights.clone();

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
            containers.working_area().size.w
        } else {
            containers.working_area().size.h
        };
        let root = Self::root(containers, idx);
        // sway edits the floating container's current pending box. While that same root owns
        // workspace fullscreen, the current box is output-sized rather than the saved
        // pre-fullscreen floating box.
        let current_area = if containers.arena().fullscreen_key() == Some(root) {
            containers
                .arena()
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
                containers.view_size().w
            } else {
                containers.view_size().h
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
        containers
            .arena_mut()
            .layout_container_subtree_in(root, area);
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
        let active = self.active_window_id(containers);
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
            match containers.arena_mut().window_key(id) {
                Some(key) => key,
                None => return,
            }
        } else {
            match self.selected_key_in(containers, idx) {
                Some(key) => key,
                None => return,
            }
        };

        if let Some(tile) = containers.arena_mut().get_tile_mut(key) {
            tile.floating_preset_width_idx = None;
        }

        let Some((parent_key, child_idx, available, child_count, _)) =
            containers.container_metrics_for(key, Layout::SplitH)
        else {
            self.resize_container_around_center(containers, idx, change, ResizeAxis::Horizontal);
            return;
        };
        if child_count <= 1 {
            self.resize_container_around_center(containers, idx, change, ResizeAxis::Horizontal);
            return;
        }

        let current_percent = containers
            .arena_mut()
            .child_percent(parent_key, child_idx)
            .unwrap_or(1.0);
        let percent = percent_from_size_change(
            current_percent,
            available,
            || containers.ppt_reference(key, Layout::SplitH),
            change,
        );

        if containers
            .arena_mut()
            .set_child_percent(parent_key, child_idx, Layout::SplitH, percent)
        {
            let _ = animate;
            let root = Self::root(containers, idx);
            containers.arena_mut().layout_branch(root);
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
        let root = Self::root(containers, idx);
        let current = if containers.arena().fullscreen_key() == Some(root) {
            containers
                .arena()
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
                containers.view_size().w
            } else {
                containers.view_size().h
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
        containers
            .arena_mut()
            .layout_container_subtree_in(root, area);
    }

    pub fn set_window_height(
        &mut self,
        containers: &mut ContainerTree<W>,
        id: Option<&W::Id>,
        change: SizeChange,
        animate: bool,
    ) {
        let active = self.active_window_id(containers);
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
            match containers.arena_mut().window_key(id) {
                Some(key) => key,
                None => return,
            }
        } else {
            match self.selected_key_in(containers, idx) {
                Some(key) => key,
                None => return,
            }
        };

        if let Some(tile) = containers.arena_mut().get_tile_mut(key) {
            tile.floating_preset_height_idx = None;
        }

        let Some((parent_key, child_idx, available, child_count, _)) =
            containers.container_metrics_for(key, Layout::SplitV)
        else {
            self.resize_container_around_center(containers, idx, change, ResizeAxis::Vertical);
            return;
        };
        if child_count <= 1 {
            self.resize_container_around_center(containers, idx, change, ResizeAxis::Vertical);
            return;
        }

        let current_percent = containers
            .arena_mut()
            .child_percent(parent_key, child_idx)
            .unwrap_or(1.0);
        let percent = percent_from_size_change(
            current_percent,
            available,
            || containers.ppt_reference(key, Layout::SplitV),
            change,
        );

        if containers
            .arena_mut()
            .set_child_percent(parent_key, child_idx, Layout::SplitV, percent)
        {
            let _ = animate;
            let root = Self::root(containers, idx);
            containers.arena_mut().layout_branch(root);
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

        let roots: Vec<_> = containers.arena().floating_roots().collect();
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
        let Some(root) = containers.arena().floating_root_at(idx) else {
            return false;
        };
        let landing = descend
            .then(|| containers.arena().focus_inactive_view(root))
            .flatten();

        self.raise_container(containers, idx, 0);
        self.bring_up_descendants_of(containers, 0);

        if let Some(landing) = landing {
            containers.arena_mut().focus_node(landing);
        } else if containers.branch_root_is_lone_window(root) {
            containers.arena_mut().focus_node(root);
        } else {
            containers.arena_mut().select_container(root);
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
        let Some(idx) = containers.arena().floating_root_index(root) else {
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
        let root = Self::root(containers, active_idx);
        let Some(position) = self.selected_key_in(containers, active_idx) else {
            return false;
        };
        let Some(parent_layout) = containers.command_target_parent_layout() else {
            return false;
        };
        let direction = match (parent_layout.is_horizontal(), forward) {
            (true, true) => Direction::Right,
            (true, false) => Direction::Left,
            (false, true) => Direction::Down,
            (false, false) => Direction::Up,
        };

        // The shared tree traversal owns every step inside the floating subtree.
        if containers.focus_along_parent(forward, descend) {
            return true;
        }
        if containers.arena().fullscreen_key().is_some() || Self::root_count(containers) <= 1 {
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
            let Some(idx) = containers.arena().floating_root_index(candidate) else {
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
        if Self::root_count(containers) <= 1 {
            return false;
        }
        if !containers
            .arena()
            .workspace_layout()
            .is_parallel_to(direction)
        {
            return false;
        }
        let direct = if direction.is_leading() {
            active_idx
                .checked_add(1)
                .filter(|idx| *idx < Self::root_count(containers))
        } else {
            active_idx.checked_sub(1)
        };
        let (target_idx, wrapped) = match direct {
            Some(idx) => (idx, false),
            None if !allow_wrap => return false,
            None if direction.is_leading() => (0, true),
            None => (Self::root_count(containers) - 1, true),
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
        let branch_root = Self::root(containers, idx);
        if containers
            .arena_mut()
            .focus_in_direction_in_branch(branch_root, direction, allow_wrap)
        {
            return true;
        }

        // The walk ran out of branch without finding anything. sway does not stop there:
        // `node_get_in_direction_tiling` climbs to the floating root through
        // `pending.parent` and takes one more step along `workspace->floating`. Only from
        // below, though — a command aimed at the root itself is the one case sway sends to
        // the geometric floating search instead, and that is the caller's fallback.
        if containers.arena().branch_position(branch_root) == Some(branch_root) {
            return false;
        }
        if containers.arena().fullscreen_key().is_some() {
            return false;
        }
        self.step_root_sibling(containers, idx, direction, true, allow_wrap)
    }

    fn focus_in_stack_order(&mut self, containers: &mut ContainerTree<W>, delta: isize) -> bool {
        if Self::root_count(containers) <= 1 {
            return false;
        }

        let Some(idx) = self.active_container_idx(containers) else {
            return false;
        };
        let len = Self::root_count(containers) as isize;
        let target_idx = (idx as isize + delta).rem_euclid(len) as usize;
        if target_idx == idx {
            return false;
        }

        let root = Self::root(containers, target_idx);
        let Some(id) = containers
            .arena_mut()
            .focused_window_in_branch(root)
            .map(|win| win.id().clone())
            .or_else(|| {
                containers
                    .arena_mut()
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
        if Self::root_count(containers) <= 1 {
            return false;
        }

        let Some(active_idx) = self.active_container_idx(containers) else {
            return false;
        };

        let mut ordered: Vec<_> = (0..Self::root_count(containers))
            .map(|idx| {
                (
                    idx,
                    containers
                        .arena()
                        .floating_root_id_at(idx)
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

        let root = Self::root(containers, target_idx);
        let Some(id) = containers
            .arena_mut()
            .focused_window_in_branch(root)
            .map(|win| win.id().clone())
            .or_else(|| {
                containers
                    .arena_mut()
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
        Self::root_count(containers) > 1
            && self.active_container_idx(containers).is_some_and(|idx| {
                containers.branch_root_is_lone_window(Self::root(containers, idx))
            })
            && containers
                .arena()
                .floating_roots()
                .all(|root| containers.branch_root_is_lone_window(root))
    }

    pub fn focus_left(&mut self, containers: &mut ContainerTree<W>) -> bool {
        if self.focus_within_active_container(containers, Direction::Left, true) {
            return true;
        }
        if containers.arena().fullscreen_key().is_some() {
            return false;
        }
        if self.should_cycle_top_level_stable_order(containers) {
            return self.focus_in_stable_container_order(containers, true);
        }
        self.focus_in_stack_order(containers, 1)
            || self.focus_directional(containers, |focus, other| focus.x - other.x)
    }

    pub fn focus_left_no_wrap(&mut self, containers: &mut ContainerTree<W>) -> bool {
        if self.focus_within_active_container(containers, Direction::Left, false) {
            return true;
        }
        if containers.arena().fullscreen_key().is_some() {
            return false;
        }
        self.focus_directional(containers, |focus, other| focus.x - other.x)
    }

    pub fn focus_window_by_id(&mut self, containers: &mut ContainerTree<W>, id: &W::Id) -> bool {
        let Some(_idx) = self.idx_of(containers, id) else {
            return false;
        };

        containers.arena_mut().clear_selection();
        let _ = containers.arena_mut().focus_window_by_id(id);
        true
    }

    pub fn focus_right(&mut self, containers: &mut ContainerTree<W>) -> bool {
        if self.focus_within_active_container(containers, Direction::Right, true) {
            return true;
        }
        if containers.arena().fullscreen_key().is_some() {
            return false;
        }
        if self.should_cycle_top_level_stable_order(containers) {
            return self.focus_in_stable_container_order(containers, false);
        }
        self.focus_in_stack_order(containers, -1)
            || self.focus_directional(containers, |focus, other| other.x - focus.x)
    }

    pub fn focus_right_no_wrap(&mut self, containers: &mut ContainerTree<W>) -> bool {
        if self.focus_within_active_container(containers, Direction::Right, false) {
            return true;
        }
        if containers.arena().fullscreen_key().is_some() {
            return false;
        }
        self.focus_directional(containers, |focus, other| other.x - focus.x)
    }

    pub fn focus_up(&mut self, containers: &mut ContainerTree<W>) -> bool {
        if self.focus_within_active_container(containers, Direction::Up, true) {
            return true;
        }
        if containers.arena().fullscreen_key().is_some() {
            return false;
        }
        self.focus_directional(containers, |focus, other| focus.y - other.y)
    }

    pub fn focus_up_no_wrap(&mut self, containers: &mut ContainerTree<W>) -> bool {
        if self.focus_within_active_container(containers, Direction::Up, false) {
            return true;
        }
        if containers.arena().fullscreen_key().is_some() {
            return false;
        }
        self.focus_directional(containers, |focus, other| focus.y - other.y)
    }

    pub fn focus_down(&mut self, containers: &mut ContainerTree<W>) -> bool {
        if self.focus_within_active_container(containers, Direction::Down, true) {
            return true;
        }
        if containers.arena().fullscreen_key().is_some() {
            return false;
        }
        self.focus_directional(containers, |focus, other| other.y - focus.y)
    }

    pub fn focus_down_no_wrap(&mut self, containers: &mut ContainerTree<W>) -> bool {
        if self.focus_within_active_container(containers, Direction::Down, false) {
            return true;
        }
        if containers.arena().fullscreen_key().is_some() {
            return false;
        }
        self.focus_directional(containers, |focus, other| other.y - focus.y)
    }

    pub fn focus_leftmost(&mut self, containers: &mut ContainerTree<W>) {
        let result = self
            .tiles_with_offsets_visible(containers)
            .min_by(|(_, pos_a), (_, pos_b)| f64::total_cmp(&pos_a.x, &pos_b.x));
        if let Some((tile, _)) = result {
            let id = tile.window().id().clone();
            self.activate_window(containers, &id);
        }
    }

    pub fn focus_rightmost(&mut self, containers: &mut ContainerTree<W>) {
        let result = self
            .tiles_with_offsets_visible(containers)
            .max_by(|(_, pos_a), (_, pos_b)| f64::total_cmp(&pos_a.x, &pos_b.x));
        if let Some((tile, _)) = result {
            let id = tile.window().id().clone();
            self.activate_window(containers, &id);
        }
    }

    pub fn focus_topmost(&mut self, containers: &mut ContainerTree<W>) {
        let result = self
            .tiles_with_offsets_visible(containers)
            .min_by(|(_, pos_a), (_, pos_b)| f64::total_cmp(&pos_a.y, &pos_b.y));
        if let Some((tile, _)) = result {
            let id = tile.window().id().clone();
            self.activate_window(containers, &id);
        }
    }

    pub fn focus_bottommost(&mut self, containers: &mut ContainerTree<W>) {
        let result = self
            .tiles_with_offsets_visible(containers)
            .max_by(|(_, pos_a), (_, pos_b)| f64::total_cmp(&pos_a.y, &pos_b.y));
        if let Some((tile, _)) = result {
            let id = tile.window().id().clone();
            self.activate_window(containers, &id);
        }
    }

    pub(super) fn focus_parent(&mut self, containers: &mut ContainerTree<W>) -> bool {
        let Some(idx) = self.active_container_idx(containers) else {
            return false;
        };
        if containers.focus_parent_is_obstructed_by_fullscreen() {
            return true;
        }
        let root = Self::root(containers, idx);
        if containers.handle_focus_parent_in_branch_fullscreen_scope(root) {
            return true;
        }

        // One step, not a walk: `select_parent_in` stops at the branch root, so there is
        // never a second ancestor to consider inside the group.
        if containers.arena_mut().select_parent_in(root) {
            return true;
        }

        // It declined, so the position already *is* the root, and above a floating root is
        // the workspace — which Workspace represents outside ContainerTree. A lone floating
        // window is its own root, so this is the first step from it too: sway has no extra
        // stop for a view that floats, because there is no container around it to stop on.
        containers.arena_mut().select_parent()
    }

    pub fn focus_child(&mut self, containers: &mut ContainerTree<W>) -> bool {
        let Some(idx) = self.active_container_idx(containers) else {
            return false;
        };
        let root = Self::root(containers, idx);
        containers
            .arena_mut()
            .selected_container_key()
            .is_some_and(|key| containers.arena_mut().is_descendant(key, root))
            && containers.arena_mut().select_child()
    }

    fn consume_or_expel_window(
        &mut self,
        containers: &mut ContainerTree<W>,
        window: Option<&W::Id>,
        direction: Direction,
    ) {
        if let Some(id) = window {
            if !self.activate_window(containers, id) {
                return;
            }
        }

        let Some(idx) = self.active_container_idx(containers) else {
            return;
        };

        if self.move_tree_command_target(containers, idx, direction) {
            return;
        }

        self.split_container(containers, idx, Layout::SplitV);
    }

    pub fn consume_or_expel_window_left(
        &mut self,
        containers: &mut ContainerTree<W>,
        window: Option<&W::Id>,
    ) {
        self.consume_or_expel_window(containers, window, Direction::Left);
    }

    pub fn consume_or_expel_window_right(
        &mut self,
        containers: &mut ContainerTree<W>,
        window: Option<&W::Id>,
    ) {
        self.consume_or_expel_window(containers, window, Direction::Right);
    }

    pub fn consume_into_column(&mut self, containers: &mut ContainerTree<W>) {
        let Some(idx) = self.active_container_idx(containers) else {
            return;
        };
        self.split_container(containers, idx, Layout::SplitV);
    }

    pub fn expel_from_column(&mut self, containers: &mut ContainerTree<W>) {
        let Some(idx) = self.active_container_idx(containers) else {
            return;
        };
        self.split_container(containers, idx, Layout::SplitH);
    }

    fn move_tree_command_target(
        &mut self,
        containers: &mut ContainerTree<W>,
        idx: usize,
        direction: Direction,
    ) -> bool {
        containers.move_in_branch(Self::root(containers, idx), direction)
    }

    pub fn set_column_display(
        &mut self,
        containers: &mut ContainerTree<W>,
        display: ColumnDisplay,
    ) {
        let target_layout = match display {
            ColumnDisplay::Normal => Layout::SplitV,
            ColumnDisplay::Tabbed => Layout::Tabbed,
        };

        let Some(idx) = self.active_container_idx(containers) else {
            return;
        };
        containers.set_layout_in_branch(Self::root(containers, idx), target_layout);
    }

    pub fn toggle_column_tabbed_display(&mut self, containers: &mut ContainerTree<W>) {
        let Some(idx) = self.active_container_idx(containers) else {
            return;
        };
        let root = Self::root(containers, idx);
        let target = match containers.selection_layout_in(root) {
            Some(Layout::Tabbed) => Layout::SplitV,
            _ => Layout::Tabbed,
        };
        containers.set_layout_in_branch(root, target);
    }

    pub fn split_horizontal(&mut self, containers: &mut ContainerTree<W>) {
        let Some(idx) = self.active_container_idx(containers) else {
            return;
        };
        self.split_container(containers, idx, Layout::SplitH);
    }

    pub fn split_vertical(&mut self, containers: &mut ContainerTree<W>) {
        let Some(idx) = self.active_container_idx(containers) else {
            return;
        };
        self.split_container(containers, idx, Layout::SplitV);
    }

    pub fn split_none(&mut self, containers: &mut ContainerTree<W>) {
        let Some(idx) = self.active_container_idx(containers) else {
            return;
        };
        let root = Self::root(containers, idx);
        let _ = containers.unsplit_in_branch(root);
    }

    /// A floating root's provenance, promoted on read.
    ///
    /// Creation decides whether a root is Tiri's addressing scaffolding or a container sway
    /// also has, and that answer is not derivable afterwards — a floated `tabbed` is a real
    /// container whether or not anything set the user-created bit on it. What *is* derivable
    /// is the one-way promotion: the moment a command makes an implicit root addressable, sway
    /// publishes it from then on. Reading that here rather than writing it back means no
    /// operation has to remember to keep a copy in step — split, layout mode, cycle, expel and
    /// whatever comes next are all right by construction.
    fn root_kind(&self, containers: &ContainerTree<W>, idx: usize) -> FloatingRootKind {
        containers
            .arena()
            .floating_root_kind_at(idx)
            .expect("floating index must have semantic provenance")
    }

    fn split_container(
        &mut self,
        containers: &mut ContainerTree<W>,
        idx: usize,
        layout: Layout,
    ) -> bool {
        let root = Self::root(containers, idx);
        if !containers.split_in_branch(root, layout) {
            return false;
        }
        // The arena replaces the authoritative root entry atomically when wrapping the old root.
        true
    }

    pub fn set_layout_mode(&mut self, containers: &mut ContainerTree<W>, layout: Layout) {
        let Some(idx) = self.active_container_idx(containers) else {
            return;
        };
        containers.set_layout_in_branch(Self::root(containers, idx), layout);
    }

    pub fn toggle_split_layout(&mut self, containers: &mut ContainerTree<W>) {
        let Some(idx) = self.active_container_idx(containers) else {
            return;
        };
        containers.toggle_split_in_branch(Self::root(containers, idx));
    }

    pub fn toggle_layout_all(&mut self, containers: &mut ContainerTree<W>) {
        let Some(idx) = self.active_container_idx(containers) else {
            return;
        };
        containers.toggle_layout_all_in_branch(Self::root(containers, idx));
    }

    pub fn set_default_layout(&mut self, containers: &mut ContainerTree<W>) {
        let Some(idx) = self.active_container_idx(containers) else {
            return;
        };
        containers.set_default_layout_in_branch(Self::root(containers, idx));
    }

    pub(super) fn toggle_layout_cycle(
        &mut self,
        containers: &mut ContainerTree<W>,
        cycle: &[LayoutCycleEntry],
    ) {
        let Some(idx) = self.active_container_idx(containers) else {
            return;
        };
        containers.toggle_layout_cycle_in_branch(Self::root(containers, idx), cycle);
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

        let root = Self::root(containers, idx);
        containers.arena_mut().layout_branch(root);

        self.interactive_resize_end(None);
    }

    fn move_by(&mut self, containers: &mut ContainerTree<W>, amount: Point<f64, Logical>) {
        let Some(active_id) = self.active_window_id(containers) else {
            return;
        };
        if self.is_fullscreen(containers, &active_id) {
            return;
        }
        let idx = self.idx_of(containers, &active_id).unwrap();

        let new_pos = self.container_area(containers, idx).loc + amount;
        self.move_container_to(containers, idx, new_pos, true)
    }

    pub fn move_left(&mut self, containers: &mut ContainerTree<W>) {
        self.move_by(containers, Point::from((-DIRECTIONAL_MOVE_PX, 0.)));
    }

    pub fn move_right(&mut self, containers: &mut ContainerTree<W>) {
        self.move_by(containers, Point::from((DIRECTIONAL_MOVE_PX, 0.)));
    }

    pub fn move_up(&mut self, containers: &mut ContainerTree<W>) {
        self.move_by(containers, Point::from((0., -DIRECTIONAL_MOVE_PX)));
    }

    pub fn move_down(&mut self, containers: &mut ContainerTree<W>) {
        self.move_by(containers, Point::from((0., DIRECTIONAL_MOVE_PX)));
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
        if self.is_fullscreen(containers, &id) {
            return;
        }
        let idx = self.idx_of(containers, &id).unwrap();

        let mut pos = self.container_area(containers, idx).loc;

        let available_width = containers.working_area().size.w;
        let available_height = containers.working_area().size.h;
        let working_area_loc = containers.working_area().loc;

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

    pub fn center_window(&mut self, containers: &mut ContainerTree<W>, id: Option<&W::Id>) {
        let Some(id) = self.resolve_target_id(containers, id) else {
            return;
        };
        if self.is_fullscreen(containers, &id) {
            return;
        }
        let idx = self.idx_of(containers, &id).unwrap();

        let new_pos = center_preferring_top_left_in_area(
            containers.working_area(),
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
            let Some(key) = containers.arena_mut().window_key(id) else {
                return false;
            };
            let Some(tile) = containers.arena_mut().get_tile_mut(key) else {
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

        let root = Self::root(containers, container_idx);
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
            let Some(key) = containers.arena_mut().window_key(id) else {
                return true;
            };
            let Some(tile) = containers.arena_mut().get_tile(key) else {
                return true;
            };
            // Fullscreen is a temporary output-sized commit, not a new floating resize base.
            // Keeping it would make unfullscreen restore the output size instead of the last
            // ordinary floating size.
            if tile.window().pending_sizing_mode().is_normal() {
                let tile_size = tile.tile_size();
                containers
                    .arena_mut()
                    .record_floating_resize_base(root, tile_size);
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
        focus_ring: bool,
        layer: RenderLayer,
    ) -> Vec<FloatingSpaceRenderElement<R>> {
        let tree = containers.arena();
        let tile_count = self.tiles(containers).count();
        let estimated_capacity = tile_count * 4 + self.closing_windows.len() + tile_count / 2;
        let mut elements = Vec::with_capacity(estimated_capacity);
        let scale = Scale::from(containers.scale());

        // Draw the closing windows on top of the other windows.
        //
        // FIXME: I guess this should rather preserve the stacking order when the window is closed.
        for closing in self
            .closing_windows
            .iter()
            .rev()
            .filter(|_| layer.is_normal())
        {
            let elem = closing.render(ctx.as_gles(), view_rect, scale);
            elements.push(elem.into());
        }

        let active = self.active_window_id(containers);
        let fullscreen_key = self.fullscreen_key(containers);
        let fullscreen_id = self.fullscreen_window_id(containers).cloned();
        let selection_is_container = self
            .active_container_idx(containers)
            .is_some_and(|idx| self.selected_is_container_in(containers, idx));

        // Like tiling, push container selection before the regular window
        // contents so it stays visually on top after the global reverse-order
        // composition pass in the renderer.
        if (focus_ring || containers.side_is_active(true))
            && selection_is_container
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
                        containers.scale(),
                        containers.side_is_active(true),
                        containers.options().layout.focus_ring,
                        containers.options().layout.border,
                        &mut |elem| {
                            elements.push(FloatingSpaceRenderElement::ContainerSelection(elem))
                        },
                    );
                }
            }
        }

        if !containers.options().layout.tab_bar.off && fullscreen_id.is_none() {
            let mut cache = containers.tab_bar_cache_mut();
            let gles = ctx.renderer.as_gles_renderer();
            let tab_bar_config = containers.options().layout.tab_bar.clone();
            let is_active_workspace = containers.side_is_active(true);
            let target = ctx.target;

            let roots: Vec<NodeKey> = if let Some(scope) = fullscreen_key {
                vec![scope]
            } else {
                tree.floating_roots().collect()
            };
            for root in roots {
                let gap = containers.branch_gap(root);
                for info in tree.tab_bar_layouts_in_branch(root) {
                    let mut info = info.clone();
                    if gap > 0.0 && info.path.is_empty() {
                        info.rect.loc.x -= gap;
                        info.rect.loc.y -= gap;
                        info.rect.size.w = (info.rect.size.w + gap * 2.0).max(0.0);
                    }
                    let state = tab_bar_state_from_info(
                        &info,
                        &tab_bar_config,
                        is_active_workspace,
                        containers.scale(),
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
                            containers.scale(),
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
                    elements.push(FloatingSpaceRenderElement::TabBar(
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
        }

        if let Some(fullscreen_id) = fullscreen_id.as_ref() {
            // Only render the fullscreen tile at (0, 0).
            if let Some(tile) = self
                .tiles(containers)
                .find(|t| t.window().id() == fullscreen_id)
            {
                let is_focused = containers.side_is_active(true);
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
            for (tile, tile_pos) in self.tiles_with_render_positions(containers) {
                // Skip tiles belonging to a different render layer.
                if layer.is_normal() == tile.is_moving_between_workspaces() {
                    continue;
                }
                if fullscreen_key.is_some_and(|scope| !tree.is_descendant(tile.node_key(), scope)) {
                    continue;
                }
                // Skip tiles entirely outside the viewport (culling)
                let tile_rect = Rectangle::new(tile_pos, tile.tile_size());
                if !tile_rect.overlaps(view_rect) {
                    continue;
                }

                let is_focused = containers.side_is_active(true)
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
    pub fn render<R: NiriRenderer>(
        &self,
        containers: &ContainerTree<W>,
        ctx: RenderCtx<R>,
        xray_pos: XrayPos,
        view_rect: Rectangle<f64, Logical>,
        focus_ring: bool,
        layer: RenderLayer,
        push: &mut dyn FnMut(FloatingSpaceRenderElement<R>),
    ) {
        for elem in self.render_elements(containers, ctx, xray_pos, view_rect, focus_ring, layer) {
            push(elem);
        }
    }

    pub fn interactive_resize_begin(
        &mut self,
        containers: &ContainerTree<W>,
        window: W::Id,
        edges: ResizeEdge,
    ) -> bool {
        let tree = containers.arena();
        if self.interactive_resize.is_some() {
            return false;
        }

        let Some(idx) = self.idx_of(containers, &window) else {
            return false;
        };

        let root = Self::root(containers, idx);
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
            super::container_tree::branch_display_layouts(containers.arena(), root)
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
                .arena_mut()
                .window_key(window)
                .and_then(|key| containers.arena_mut().get_tile(key))
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
                let root = Self::root(containers, idx);
                containers.arena_mut().layout_branch(root);
            }
        }

        true
    }

    /// Drop a resize whose window has left the floating side.
    ///
    /// Every route that takes a window off this side already cancels its resize — unfloating,
    /// removing it, emptying its group. A swap across the floating boundary is the one that
    /// does not, because it happens down in the arena and never reaches this struct. It also
    /// moves two nodes at once and either could be the one being resized, so this asks whether
    /// the resize still has a tile rather than taking a key from the caller.
    pub(super) fn forget_resize_that_left(&mut self, containers: &ContainerTree<W>) {
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
        let _span = tracy_client::span!("FloatingSpace::refresh");
        let active = self.active_window_id(containers);
        let deactivate_unfocused = containers.options().deactivate_unfocused_windows;
        let disable_resize_throttling = containers.options().disable_resize_throttling;
        let border_base = containers.options().layout.border;
        let working_area_size = containers.working_area().size;
        let resize_target = self.interactive_resize.as_ref().and_then(|resize| {
            let idx = self.idx_of(containers, &resize.window)?;
            let mut ids = Vec::new();
            let root = Self::root(containers, idx);
            for tile in containers.arena_mut().tiles_in_branch(root) {
                ids.push(tile.window().id().clone());
            }
            Some((resize.data, ids))
        });
        for tile in self.tiles_mut(containers) {
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
            let root = Self::root(containers, idx);
            for tile in containers.arena_mut().tiles_in_branch_mut(root) {
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
            .options()
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

        let width = resolve(width, containers.working_area().size.w);
        let height = resolve(height, containers.working_area().size.h);

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

    #[cfg(test)]
    pub fn wrapper_selected_for_window(&self, containers: &ContainerTree<W>, id: &W::Id) -> bool {
        let tree = containers.arena();
        self.idx_of(containers, id)
            .is_some_and(|idx| tree.selected_container_key() == Some(Self::root(containers, idx)))
    }

    #[cfg(test)]
    pub fn root_layout_for_window(
        &self,
        containers: &ContainerTree<W>,
        id: &W::Id,
    ) -> Option<Layout> {
        let tree = containers.arena();
        let idx = self.idx_of(containers, id)?;
        tree.branch_layout(Self::root(containers, idx))
    }

    #[cfg(test)]
    pub fn debug_tree_for_window(&self, containers: &ContainerTree<W>, id: &W::Id) -> Option<String>
    where
        W::Id: std::fmt::Display,
    {
        let tree = containers.arena();
        let idx = self.idx_of(containers, id)?;
        Some(tree.debug_branch(Self::root(containers, idx)))
    }

    pub fn verify_invariants(&self, containers: &ContainerTree<W>) {
        use std::collections::HashSet;

        let tree = containers.arena();
        assert!(containers.scale() > 0.);
        assert!(containers.scale().is_finite());

        let stack_roots: HashSet<_> = tree.floating_roots().collect();
        assert_eq!(
            stack_roots.len(),
            tree.floating_root_count(),
            "the floating stack must not contain duplicate roots"
        );
        let stack_ids: HashSet<_> = (0..tree.floating_root_count())
            .map(|idx| tree.floating_root_id_at(idx).expect("floating root id"))
            .collect();
        assert_eq!(
            stack_ids.len(),
            tree.floating_root_count(),
            "the floating stack must not contain duplicate reinsertion ids"
        );

        for root in tree.floating_roots() {
            use crate::layout::SizingMode;

            assert!(
                tree.holds_node(root),
                "a floating root must exist in the arena"
            );
            assert_eq!(tree.parent_of(root), Some(tree.workspace_root()));
            tree.floating_container_area(root)
                .expect("every floating root must own its geometry");
            for tile in tree.tiles_in_branch(root) {
                assert!(Rc::ptr_eq(containers.options(), &tile.options));
                assert_eq!(containers.view_size(), tile.view_size());
                assert_eq!(*containers.clock(), tile.clock);
                assert_eq!(containers.scale(), tile.scale());
                tile.verify_invariants();

                if let Some(idx) = tile.floating_preset_width_idx {
                    assert!(idx < containers.options().layout.preset_column_widths.len());
                }
                if let Some(idx) = tile.floating_preset_height_idx {
                    assert!(idx < containers.options().layout.preset_window_heights.len());
                }

                let is_fullscreen_tile = tree.window_owns_fullscreen(tile.window().id());
                if !is_fullscreen_tile {
                    assert_eq!(
                        tile.window().pending_sizing_mode(),
                        SizingMode::Normal,
                        "floating windows cannot be maximized or fullscreen"
                    );
                }
            }
        }

        if let Some(id) = self.active_window_id(containers) {
            assert!(tree.floating_root_count() > 0);
            assert!(
                self.contains(containers, &id),
                "active window must be present in tiles"
            );
        } else {
            assert_eq!(tree.floating_root_count(), 0);
        }

        if let Some(resize) = &self.interactive_resize {
            assert!(
                self.contains(containers, &resize.window),
                "interactive resize window must be present in tiles"
            );
        }
    }
}

impl<W: LayoutElement> FloatingSpace<W> {
    pub(crate) fn layout_tree_nodes(&self, containers: &ContainerTree<W>) -> Vec<LayoutTreeNode> {
        let tree = containers.arena();
        // Rendering keeps the topmost floating group first. sway's `floating_nodes` IPC list
        // exposes the workspace list in the opposite, bottom-to-top order. Reverse only this
        // projection: changing the stored order would invert hit testing, focus raising and
        // rendering merely to make IPC look right.
        let roots: Vec<_> = tree.floating_roots().collect();
        roots
            .into_iter()
            .enumerate()
            .rev()
            .enumerate()
            .filter_map(|(ipc_idx, (stack_idx, root_key))| {
                let focused_key = tree
                    .selected_node_key()
                    .filter(|key| tree.is_descendant(*key, root_key));
                let mut path = vec![ipc_idx];
                let mut root =
                    tree.layout_tree_for_branch(root_key, focused_key, &mut path, true)?;
                root.floating_root_kind = Some(self.root_kind(containers, stack_idx).ipc());
                containers.apply_workspace_fullscreen(root)
            })
            .collect()
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
