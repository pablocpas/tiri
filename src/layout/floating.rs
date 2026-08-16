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
    floating_position_from_logical, scale_floating_position, Direction, InactiveTilingReference,
    InsertParentInfo, Layout, NodeKey, TabBarInfo,
};
use super::focus_ring::{
    render_container_selection, ContainerSelectionStyle, FocusRingEdges, FocusRingRenderElement,
};
use super::legacy_column::ColumnWidth;
use super::tile::{Tile, TileRenderElement, TileRenderSnapshot};
use super::tree_space::{percent_from_size_change, LeafFrameInfo, TileConfig, TreeSpace};
use super::workspace::{InteractiveResize, ResolvedSize};
use super::{
    resize_edges_for_point, ConfigureIntent, InteractiveResizeData, LayoutCycleEntry,
    LayoutElement, Options, RemovedTile, ResizeAxis, ResizeRequest, SizeFrac,
};
use crate::animation::Animation;
use crate::layout::tab_bar::{
    render_tab_bar, tab_bar_state_from_info, TabBarCacheEntry, TabBarRenderOutput,
};
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
/// The groups themselves live in the workspace's space, which is why nothing here owns one.
/// A container crossing between the two lists is a move, not a reconstruction, so its key —
/// and its place in the seat's order — survives the crossing the way it does in sway.
#[derive(Debug)]
pub struct FloatingSpace<W: LayoutElement> {
    /// Floating groups in top-to-bottom order.
    containers: Vec<FloatingContainer>,

    /// Next floating container id.
    next_container_id: u64,

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

/// One floating group: which branch of the workspace tree it is, and where it sits.
#[derive(Debug)]
struct FloatingContainer {
    id: u64,
    /// The branch's root — sway's entry in `ws->floating`.
    root: NodeKey,
    /// Provenance as of creation. Read it through [`FloatingSpace::root_kind`], which
    /// promotes an implicit root the tree has since made addressable.
    kind: FloatingRootKind,
}

/// Semantic provenance of a floating root.
///
/// An implicit one-window group is Tiri's addressing scaffolding. The other two variants are
/// real containers that sway also publishes, even when they contain only one window.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum FloatingRootKind {
    ImplicitWindowGroup,
    FloatedContainer,
    WorkspaceWrapper,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum FloatingResizeAnchor {
    Center,
    KeepOrigin,
}

impl FloatingRootKind {
    fn is_workspace_wrapper(self) -> bool {
        self == Self::WorkspaceWrapper
    }

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
    floating: &'a FloatingSpace<W>,
    space: &'a TreeSpace<W>,
) -> impl Iterator<Item = &'a Tile<W>> + 'a {
    let tree = space.tree();
    floating
        .containers
        .iter()
        .flat_map(|container| tree.tiles_in_branch(container.root))
}

/// All tiles across the floating containers (mutable), in container order.
fn floating_tile_iter_mut<'a, W: LayoutElement>(
    floating: &'a mut FloatingSpace<W>,
    space: &'a mut TreeSpace<W>,
) -> impl Iterator<Item = &'a mut Tile<W>> + 'a {
    let keys: Vec<NodeKey> = floating
        .containers
        .iter()
        .flat_map(|container| space.tree().leaf_keys_in_branch(container.root))
        .collect();
    space
        .tree_mut()
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
            containers: Vec::new(),
            next_container_id: 1,
            interactive_resize: None,
            closing_windows: Vec::new(),
            render_layout_scratch: Vec::new(),
        }
    }

    fn container_area(&self, space: &TreeSpace<W>, idx: usize) -> Rectangle<f64, Logical> {
        let root = self.containers[idx].root;
        space
            .tree()
            .floating_container_area(root)
            .expect("every floating stack entry must name a floating root")
    }

    fn set_container_logical_pos(
        &mut self,
        space: &mut TreeSpace<W>,
        idx: usize,
        logical_pos: Point<f64, Logical>,
    ) -> Point<f64, Logical> {
        space
            .tree_mut()
            .set_floating_logical_pos(self.containers[idx].root, logical_pos)
    }

    fn set_container_size(
        &mut self,
        space: &mut TreeSpace<W>,
        idx: usize,
        size: Size<f64, Logical>,
    ) -> Rectangle<f64, Logical> {
        space
            .tree_mut()
            .set_floating_size(self.containers[idx].root, size)
    }

    pub fn update_config(
        &mut self,
        space: &mut TreeSpace<W>,
        view_size: Size<f64, Logical>,
        working_area: Rectangle<f64, Logical>,
        scale: f64,
        options: Rc<Options>,
    ) {
        let tree = space.tree_mut();
        for container in &self.containers {
            tree.update_floating_working_area(container.root, working_area);
        }
        tree.layout();

        for tile in self.tiles_mut(space) {
            tile.update_config(view_size, scale, options.clone());
        }
    }

    pub fn update_shaders(&mut self, space: &mut TreeSpace<W>) {
        for tile in self.tiles_mut(space) {
            tile.update_shaders();
        }
    }

    pub fn advance_animations(&mut self, space: &mut TreeSpace<W>) {
        for tile in self.tiles_mut(space) {
            tile.advance_animations();
        }

        self.closing_windows.retain_mut(|closing| {
            closing.advance_animations();
            closing.are_animations_ongoing()
        });
    }

    pub fn are_animations_ongoing(&self, space: &TreeSpace<W>) -> bool {
        self.tiles(space).any(Tile::are_animations_ongoing) || !self.closing_windows.is_empty()
    }

    pub fn are_transitions_ongoing(&self, space: &TreeSpace<W>) -> bool {
        self.tiles(space).any(Tile::are_transitions_ongoing) || !self.closing_windows.is_empty()
    }

    pub fn update_render_elements(
        &mut self,
        space: &mut TreeSpace<W>,
        is_active: bool,
        view_rect: Rectangle<f64, Logical>,
    ) {
        let _span = tracy_client::span!("FloatingSpace::update_render_elements");
        let active = self.active_window_id(space);
        let fullscreen_id = self.fullscreen_window_id(space).cloned();
        let selection_is_container = self
            .active_container_idx(space)
            .is_some_and(|idx| self.selected_is_container_in(space, idx));
        let scale = space.scale();
        let floating_has_focus = space.side_is_active(true);
        let applied = space.tree_mut().apply_pending_layouts_if_ready();
        if applied && space.tree_mut().take_pending_relayout() {
            space.tree_mut().layout();
        }
        let mut layouts = std::mem::take(&mut self.render_layout_scratch);
        for container in &mut self.containers {
            layouts.clear();
            {
                let _span =
                    tracy_client::span!("FloatingSpace::project_display_layouts_for_render");
                layouts.extend(
                    super::tree_space::branch_display_layouts(space.tree(), container.root)
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
            let float_has_sublayout = space.tree().window_count_in_branch(container.root) > 1;
            for info in layouts.iter().copied() {
                let is_focus_head = float_has_sublayout && space.tree().is_focus_head(info.key);
                if let Some(tile) = space.tree_mut().get_tile_mut(info.key) {
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
                    // painted this space's active window `focused` while a tiled window
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
                        FocusRingEdges::all(),
                        None,
                        tile_view_rect,
                    );
                }
            }
        }
        self.render_layout_scratch = layouts;
    }

    pub fn tiles<'a>(&'a self, space: &'a TreeSpace<W>) -> impl Iterator<Item = &'a Tile<W>> + 'a {
        floating_tile_iter(self, space)
    }

    pub fn tiles_mut<'a>(
        &'a mut self,
        space: &'a mut TreeSpace<W>,
    ) -> impl Iterator<Item = &'a mut Tile<W>> + 'a {
        floating_tile_iter_mut(self, space)
    }

    pub fn tiles_with_offsets<'a>(
        &'a self,
        space: &'a TreeSpace<W>,
    ) -> impl Iterator<Item = (&'a Tile<W>, Point<f64, Logical>)> + 'a {
        let tree = space.tree();
        let mut tiles = Vec::new();
        for container in &self.containers {
            for info in super::tree_space::branch_display_layouts(space.tree(), container.root) {
                if let Some(tile) = tree.get_tile(info.key) {
                    tiles.push((tile, info.rect.loc));
                }
            }
        }
        tiles.into_iter()
    }

    pub(super) fn resize_hit_under(
        &self,
        space: &TreeSpace<W>,
        pos: Point<f64, Logical>,
    ) -> FloatingResizeResult<W::Id> {
        let tree = space.tree();
        if self.has_fullscreen_window(space) {
            return FloatingResizeResult::None;
        }

        let scale = Scale::from(space.scale());
        for container in &self.containers {
            let container_area = tree
                .floating_container_area(container.root)
                .expect("every floating stack entry must name a floating root");
            let gap = space.branch_gap(container.root);
            let tab_bar_infos = tree.tab_bar_layouts_in_branch(container.root);
            for info in super::tree_space::branch_display_layouts(space.tree(), container.root)
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
                    space.scale(),
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
        space: &TreeSpace<W>,
        pos: Point<f64, Logical>,
    ) -> Option<ResizeEdge> {
        match self.resize_hit_under(space, pos) {
            FloatingResizeResult::Hit(hit) => Some(hit.edges),
            FloatingResizeResult::Blocked => Some(ResizeEdge::empty()),
            FloatingResizeResult::None => None,
        }
    }

    fn tiles_with_offsets_visible<'a>(
        &'a self,
        space: &'a TreeSpace<W>,
    ) -> impl Iterator<Item = (&'a Tile<W>, Point<f64, Logical>)> + 'a {
        let tree = space.tree();
        let mut tiles = Vec::new();
        for container in &self.containers {
            for info in super::tree_space::branch_display_layouts(space.tree(), container.root)
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
        space: &'a mut TreeSpace<W>,
    ) -> impl Iterator<Item = (&'a mut Tile<W>, Point<f64, Logical>)> + 'a {
        let mut keys = Vec::new();
        let mut locs = Vec::new();
        for container in &self.containers {
            for info in super::tree_space::branch_display_layouts(space.tree(), container.root) {
                keys.push(info.key);
                locs.push(info.rect.loc);
            }
        }
        space
            .tree_mut()
            .tiles_mut_for_keys(&keys)
            .into_iter()
            .map(move |(idx, tile)| (tile, locs[idx]))
    }

    pub fn tiles_with_render_positions<'a>(
        &'a self,
        space: &'a TreeSpace<W>,
    ) -> impl Iterator<Item = (&'a Tile<W>, Point<f64, Logical>)> + 'a {
        let scale = space.scale();
        self.tiles_with_offsets_visible(space)
            .map(move |(tile, offset)| {
                let pos = offset + tile.render_offset();
                // Round to physical pixels.
                let pos = pos.to_physical_precise_round(scale).to_logical(scale);
                (tile, pos)
            })
    }

    fn tab_bar_hit<'a>(
        &'a self,
        space: &'a TreeSpace<W>,
        pos: Point<f64, Logical>,
    ) -> Option<(&'a W, super::HitType)> {
        // A 1px pad makes the floating bar's edges forgiving to hit: next to them is the
        // desktop, not another window.
        self.containers
            .iter()
            .find_map(|container| space.branch_tab_bar_hit(container.root, pos, 1))
    }

    pub fn window_under<'a>(
        &'a self,
        space: &'a TreeSpace<W>,
        pos: Point<f64, Logical>,
    ) -> Option<(&'a W, super::HitType)> {
        if let Some(fullscreen_id) = self.fullscreen_window_id(space) {
            let tile = self
                .tiles(space)
                .find(|t| t.window().id() == fullscreen_id)?;
            return super::HitType::hit_tile(tile, Point::from((0.0, 0.0)), pos);
        }

        let fullscreen_scope = self.fullscreen_key(space);
        let tab_hit = if let Some(scope) = fullscreen_scope {
            space.branch_tab_bar_hit(scope, pos, 1)
        } else {
            self.tab_bar_hit(space, pos)
        };
        if let Some(hit) = tab_hit {
            return Some(hit);
        }

        for (tile, tile_pos) in self.tiles_with_render_positions(space) {
            if fullscreen_scope
                .is_some_and(|scope| !space.tree().is_descendant(tile.node_key(), scope))
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
        space: &'a mut TreeSpace<W>,
        round: bool,
    ) -> impl Iterator<Item = (&'a mut Tile<W>, Point<f64, Logical>)> + 'a {
        let scale = space.scale();
        self.tiles_with_offsets_mut(space)
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
        space: &'a TreeSpace<W>,
    ) -> impl Iterator<Item = (&'a Tile<W>, WindowLayout)> + 'a {
        let scale = space.scale();
        self.tiles_with_offsets(space).map(move |(tile, offset)| {
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
        space: &TreeSpace<W>,
        rules: &ResolvedWindowRules,
    ) -> Size<i32, Logical> {
        let border_config = space.options().layout.border.merged_with(&rules.border);
        compute_toplevel_bounds(border_config, space.working_area().size)
    }

    /// Returns the geometry of the active window relative to and clamped to the working area.
    ///
    /// During animations, assumes the final tile position.
    pub fn active_window_visual_rectangle(
        &self,
        space: &TreeSpace<W>,
    ) -> Option<Rectangle<f64, Logical>> {
        let active_id = self.active_window_id(space)?;
        let (tile, offset) = self
            .tiles_with_offsets_visible(space)
            .find(|(tile, _)| tile.window().id() == &active_id)?;

        let window_pos = offset + tile.window_loc();
        let window_size = tile.window_size();
        let window_rect = Rectangle::new(window_pos, window_size);

        space.working_area().intersection(window_rect)
    }

    pub fn popup_target_rect(
        &self,
        space: &TreeSpace<W>,
        id: &W::Id,
    ) -> Option<Rectangle<f64, Logical>> {
        for (tile, pos) in self.tiles_with_offsets_visible(space) {
            if tile.window().id() == id {
                // Position within the working area.
                let mut target = space.working_area();
                target.loc -= pos;
                target.loc -= tile.window_loc();

                return Some(target);
            }
        }
        None
    }

    fn idx_of(&self, space: &TreeSpace<W>, id: &W::Id) -> Option<usize> {
        let tree = space.tree();
        let key = tree.window_key(id)?;
        let root = tree.branch_root(key);
        self.containers
            .iter()
            .position(|container| container.root == root)
    }

    #[cfg(test)]
    fn contains(&self, space: &TreeSpace<W>, id: &W::Id) -> bool {
        self.idx_of(space, id).is_some()
    }

    /// The focused floating view, or the floating MRU while tiling has keyboard focus.
    ///
    /// Focus belongs to the workspace tree's seat. `FloatingSpace` deliberately owns no
    /// projection of it; the only ordering kept here is visual stacking.
    fn active_window_id(&self, space: &TreeSpace<W>) -> Option<W::Id> {
        space.active_floating_window_id()
    }

    /// Floating projection of the workspace's single fullscreen node.
    pub(super) fn fullscreen_key(&self, space: &TreeSpace<W>) -> Option<NodeKey> {
        let tree = space.tree();
        let key = tree.fullscreen_key()?;
        (tree.holds_node(key) && tree.is_floating(key)).then_some(key)
    }

    /// Floating leaf that owns fullscreen at the client protocol boundary.
    fn fullscreen_window_id<'a>(&self, space: &'a TreeSpace<W>) -> Option<&'a W::Id> {
        self.fullscreen_key(space)?;
        space.tree().fullscreen_leaf_window_id()
    }

    fn active_container_idx(&self, space: &TreeSpace<W>) -> Option<usize> {
        let active_id = self.active_window_id(space)?;
        self.idx_of(space, &active_id)
    }

    fn selected_is_container_in(&self, space: &TreeSpace<W>, idx: usize) -> bool {
        space.selected_container_in(self.containers[idx].root)
    }

    /// The node a command targets inside container `idx`: its root when the whole floating
    /// wrapper is selected, otherwise the tree's own selection.
    fn selected_key_in(&self, space: &TreeSpace<W>, idx: usize) -> Option<NodeKey> {
        let tree = space.tree();
        tree.branch_position(self.containers[idx].root)
    }

    fn tile_at_mut<'a>(&self, space: &'a mut TreeSpace<W>, id: &W::Id) -> Option<&'a mut Tile<W>> {
        let tree = space.tree_mut();
        let key = tree.window_key(id)?;
        tree.is_floating(key)
            .then(|| tree.get_tile_mut(key))
            .flatten()
    }

    pub fn active_window<'a>(&self, space: &'a TreeSpace<W>) -> Option<&'a W> {
        let tree = space.tree();
        let id = self.active_window_id(space)?;
        let key = tree.window_key(&id)?;
        tree.is_floating(key)
            .then(|| tree.get_tile(key).map(Tile::window))
            .flatten()
    }

    pub fn active_window_mut<'a>(&self, space: &'a mut TreeSpace<W>) -> Option<&'a mut W> {
        let id = self.active_window_id(space)?;
        let tree = space.tree_mut();
        let key = tree.window_key(&id)?;
        tree.is_floating(key)
            .then(|| tree.get_tile_mut(key).map(Tile::window_mut))
            .flatten()
    }

    pub fn has_window(&self, space: &TreeSpace<W>, id: &W::Id) -> bool {
        let tree = space.tree();
        tree.window_key(id).is_some_and(|key| tree.is_floating(key))
    }

    pub fn is_empty(&self) -> bool {
        self.containers.is_empty()
    }

    pub fn set_fullscreen(
        &mut self,
        space: &mut TreeSpace<W>,
        window: &W::Id,
        is_fullscreen: bool,
    ) {
        if is_fullscreen {
            if self.is_fullscreen(space, window) {
                return;
            }

            if let Some(previous) = self.fullscreen_window_id(space).cloned() {
                if previous != *window {
                    self.set_fullscreen(space, &previous, false);
                }
            }

            let Some(key) = space
                .tree()
                .window_key(window)
                .filter(|key| space.tree().is_floating(*key))
            else {
                return;
            };

            // Store the floating size before going fullscreen.
            if let Some(tile) = self.tiles_mut(space).find(|t| t.window().id() == window) {
                Self::store_floating_size_for_restore(tile);
                tile.request_fullscreen(true, None);
            }

            space.tree_mut().set_fullscreen_key(Some(key));
        } else {
            if !self.is_fullscreen(space, window) {
                return;
            }

            // A one-window floating root records client commits as its resize base without
            // retargeting an in-flight compositor size. Fullscreen is precisely such a target;
            // restore the live base now so the ordinary floating arrange below cannot overwrite
            // the saved client size with the pre-fullscreen target.
            if let Some(idx) = self.idx_of(space, window) {
                let restore_size = self.container_area(space, idx).size;
                self.set_container_size(space, idx, restore_size);
            }

            // Restore the floating size.
            if let Some(tile) = self.tiles_mut(space).find(|t| t.window().id() == window) {
                let size = tile.floating_window_size.unwrap_or_default();
                tile.window_mut().request_size_once(size, true);
            }

            space.tree_mut().set_fullscreen_key(None);
        }
        space.tree_mut().layout();
    }

    pub fn is_fullscreen(&self, space: &TreeSpace<W>, window: &W::Id) -> bool {
        self.fullscreen_window_id(space)
            .is_some_and(|id| id == window)
    }

    pub fn has_fullscreen_window(&self, space: &TreeSpace<W>) -> bool {
        self.fullscreen_key(space).is_some()
    }

    pub fn selected_is_container(&self, space: &TreeSpace<W>, id: Option<&W::Id>) -> bool {
        let active = self.active_window_id(space);
        let Some(id) = id.or(active.as_ref()) else {
            return false;
        };
        let Some(idx) = self.idx_of(space, id) else {
            return false;
        };
        self.selected_is_container_in(space, idx)
    }

    pub(super) fn active_wrapper_selected(&self, space: &TreeSpace<W>) -> bool {
        let tree = space.tree();
        let Some(idx) = self.active_container_idx(space) else {
            return false;
        };
        tree.selected_container_key() == Some(self.containers[idx].root)
    }

    pub(super) fn close_window_ids_for_active_selection(&self, space: &TreeSpace<W>) -> Vec<W::Id> {
        let Some(idx) = self.active_container_idx(space) else {
            return Vec::new();
        };
        space.close_window_ids_in_branch(self.containers[idx].root)
    }

    pub(super) fn select_wrapper_for_window(&self, space: &mut TreeSpace<W>, id: &W::Id) -> bool {
        let Some(idx) = self.idx_of(space, id) else {
            return false;
        };
        space.tree_mut().select_container(self.containers[idx].root)
    }

    pub fn clear_selection_context(&self, space: &mut TreeSpace<W>) {
        let tree = space.tree_mut();
        tree.clear_selection();
    }

    pub fn add_tile(&mut self, space: &mut TreeSpace<W>, tile: Tile<W>, activate: bool) {
        self.add_tile_at(space, 0, tile, activate);
    }

    pub fn add_tile_with_restore_hint(
        &mut self,
        space: &mut TreeSpace<W>,
        mut tile: Tile<W>,
        activate: bool,
    ) {
        let hint = tile.floating_reinsert_hint.take();

        if let Some((container_id, insert_info)) = hint {
            if let Some(idx) = self
                .containers
                .iter()
                .position(|container| container.id == container_id)
            {
                self.add_tile_to_container_idx_with_parent_info(
                    space,
                    idx,
                    tile,
                    activate,
                    &insert_info,
                );
                return;
            }
        }

        self.add_tile(space, tile, activate);
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
        space: &mut TreeSpace<W>,
        mut idx: usize,
        mut tile: Tile<W>,
        activate: bool,
    ) {
        let config = space.tile_config();
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
        for (i, container) in self.containers.iter().enumerate().take(idx) {
            if space
                .tree_mut()
                .windows_in_branch(container.root)
                .iter()
                .any(|parent| tile.window().is_child_of(parent))
            {
                idx = i;
                break;
            }
        }

        let tile_size = requested_tile_size.unwrap_or_else(|| tile.tile_size());
        let pos = self
            .stored_or_default_tile_pos(space.working_area(), &tile)
            .unwrap_or_else(|| center_preferring_top_left_in_area(space.working_area(), tile_size));
        let rect = Rectangle::new(pos, tile_size);

        let (root, leaf) = space.tree_mut().float_new_group(tile, rect);
        // `workspace->fullscreen` names the node on whichever list it lives.
        if maps_fullscreen && space.tree().fullscreen_key().is_none() {
            space.tree_mut().set_fullscreen_key(Some(leaf));
        }
        if activate || space.tree().focused_node_key().is_none() {
            space.tree_mut().focus_node(leaf);
        }
        space.tree_mut().layout();

        let container = FloatingContainer {
            id: self.next_container_id,
            root,
            kind: FloatingRootKind::ImplicitWindowGroup,
        };
        self.next_container_id += 1;

        self.containers.insert(idx, container);
        self.bring_up_descendants_of(space, idx);
    }

    pub(super) fn add_tile_to_active_container(
        &mut self,
        space: &mut TreeSpace<W>,
        tile: Tile<W>,
        activate: bool,
    ) -> bool {
        let Some(idx) = self.active_container_idx(space) else {
            return false;
        };
        self.add_tile_to_container_idx(space, idx, tile, activate)
    }

    pub(super) fn add_tile_to_container_of(
        &mut self,
        space: &mut TreeSpace<W>,
        id: &W::Id,
        tile: Tile<W>,
        activate: bool,
    ) -> bool {
        let Some(idx) = self.idx_of(space, id) else {
            return false;
        };

        self.add_tile_to_container_idx(space, idx, tile, activate)
    }

    fn add_tile_to_container_idx(
        &mut self,
        space: &mut TreeSpace<W>,
        idx: usize,
        mut tile: Tile<W>,
        activate: bool,
    ) -> bool {
        let config = space.tile_config();
        let (win_id, _) = Self::prepare_tile_for_floating(&config, &mut tile);
        let root = self.containers[idx].root;
        if space.tree_mut().selected_container_key() == Some(root) {
            let insert_idx = space.tree_mut().branch_children_len(root);
            space
                .tree_mut()
                .insert_leaf_into_branch(root, insert_idx, tile, activate);
        } else {
            space
                .tree_mut()
                .insert_window_into_branch(root, tile, activate);
        }
        space.tree_mut().layout();

        if activate {
            self.activate_window(space, &win_id);
        }

        true
    }

    fn add_tile_to_container_idx_with_parent_info(
        &mut self,
        space: &mut TreeSpace<W>,
        idx: usize,
        mut tile: Tile<W>,
        activate: bool,
        info: &InsertParentInfo,
    ) {
        let config = space.tile_config();
        let (win_id, _) = Self::prepare_tile_for_floating(&config, &mut tile);

        let root = self.containers[idx].root;
        let _ = space
            .tree_mut()
            .insert_leaf_with_parent_info(root, info, tile, activate);
        space.tree_mut().layout();

        if activate {
            self.activate_window(space, &win_id);
        }
    }

    pub(super) fn active_container_allows_splits(&self, space: &TreeSpace<W>) -> bool {
        let tree = space.tree();
        let Some(_idx) = self.active_container_idx(space) else {
            return false;
        };
        tree.focused_container_allows_splits()
    }

    /// Whether the window named here sits in a container a new sibling can join.
    ///
    /// It used to answer about the *focused* container instead, which is the same node only
    /// when the named window happens to be the focused one. A floating window that is its own
    /// root has no container to join, and reading someone else's answer for it sent a new tile
    /// into a group that was not there.
    pub(super) fn container_allows_splits(&self, space: &TreeSpace<W>, id: &W::Id) -> bool {
        let tree = space.tree();
        let Some(key) = tree.window_key(id) else {
            return false;
        };
        tree.container_of_allows_splits(key)
    }

    pub(super) fn container_pos(
        &self,
        space: &TreeSpace<W>,
        id: &W::Id,
    ) -> Option<Point<f64, Logical>> {
        let idx = self.idx_of(space, id)?;
        Some(self.container_area(space, idx).loc)
    }

    pub(super) fn move_container_for_window_to(
        &mut self,
        space: &mut TreeSpace<W>,
        id: &W::Id,
        pos: Point<f64, Logical>,
        animate: bool,
    ) -> bool {
        let Some(idx) = self.idx_of(space, id) else {
            return false;
        };
        self.move_container_to(space, idx, pos, animate);
        true
    }

    pub fn add_tile_above(
        &mut self,
        space: &mut TreeSpace<W>,
        above: &W::Id,
        mut tile: Tile<W>,
        activate: bool,
    ) {
        let idx = self.idx_of(space, above).unwrap();

        let above_area = self.container_area(space, idx);
        let tile_size = tile.tile_size();
        let pos =
            above_area.loc + (above_area.size.to_point() - tile_size.to_point()).downscale(2.);
        let pos = self.clamp_within_working_area(space.working_area(), pos, tile_size);
        tile.floating_pos = Some(self.logical_to_size_frac(space.working_area(), pos));

        self.add_tile_at(space, idx, tile, activate);
    }

    pub(super) fn add_subtree(
        &mut self,
        space: &mut TreeSpace<W>,
        key: NodeKey,
        mut rect: Rectangle<f64, Logical>,
        activate: bool,
        focus: Option<&W::Id>,
        workspace_floated: bool,
    ) -> bool {
        let config = space.tile_config();
        let fullscreen_key = space.tree().fullscreen_key();
        let fullscreen_restore_size = fullscreen_key
            .filter(|fullscreen| *fullscreen == key)
            .and_then(|fullscreen| space.tree().fullscreen_restore_geometry(fullscreen))
            .map(|rect| rect.size);
        let mut prepared_leaf = None;
        let mut preserved_fullscreen_leaf = false;
        if space.tree_mut().is_leaf(key) {
            if let Some(tile) = space.tree_mut().get_tile_mut(key) {
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
                    if tile.floating_window_size.is_none() {
                        let size = tile.window().natural_size();
                        tile.floating_window_size =
                            Some(Self::floating_constraints(&config, tile, size));
                    }
                    let (_, requested) = Self::prepare_tile_for_floating(&config, tile);
                    prepared_leaf = Some((key, requested));
                }
            }
            if !preserved_fullscreen_leaf {
                let requested = prepared_leaf.and_then(|(_, requested)| requested);
                let working_area = space.working_area();
                if let Some(tile) = space.tree().get_tile(key) {
                    let size = requested.unwrap_or_else(|| tile.tile_size());
                    let pos = self
                        .stored_or_default_tile_pos(working_area, tile)
                        .unwrap_or_else(|| center_preferring_top_left_in_area(working_area, size));
                    rect = Rectangle::new(pos, size);
                }
            }
        }

        let area = rect;
        let root = if workspace_floated {
            space.tree_mut().float_whole_workspace(area)
        } else {
            space.tree_mut().float_as_group(key, area)
        };
        let Some(root) = root else {
            return false;
        };

        let keys = space.tree_mut().leaf_keys_in_branch(root);
        for key in keys {
            if prepared_leaf.is_some_and(|(prepared, _)| prepared == key) {
                continue;
            }
            let fullscreen_restore_size = (fullscreen_key == Some(key))
                .then(|| space.tree().fullscreen_restore_geometry(key))
                .flatten()
                .map(|rect| rect.size);
            if let Some(tile) = space.tree_mut().get_tile_mut(key) {
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
        if activate {
            if let Some(id) = focus {
                space.tree_mut().focus_window_by_id(id);
            }
        }
        space.tree_mut().layout();

        let container = FloatingContainer {
            id: self.next_container_id,
            root,
            kind: if workspace_floated {
                FloatingRootKind::WorkspaceWrapper
            } else if prepared_leaf.is_some() {
                FloatingRootKind::ImplicitWindowGroup
            } else {
                FloatingRootKind::FloatedContainer
            },
        };
        self.next_container_id += 1;

        self.containers.insert(0, container);
        self.bring_up_descendants_of(space, 0);
        true
    }

    fn bring_up_descendants_of(&mut self, space: &TreeSpace<W>, idx: usize) {
        let tree = space.tree();
        let base_windows = tree.windows_in_branch(self.containers[idx].root);
        let mut seen_windows = base_windows;
        let mut descendants: Vec<usize> = Vec::new();

        for (i, container_below) in self.containers.iter().enumerate().skip(idx + 1).rev() {
            let windows = tree.windows_in_branch(container_below.root);
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
            self.raise_container(descendant_idx, idx);
            idx += 1;
        }
    }

    pub fn remove_active_tile(&mut self, space: &mut TreeSpace<W>) -> Option<RemovedTile<W>> {
        let id = self.active_window_id(space)?;
        Some(self.remove_tile(space, &id))
    }

    pub fn remove_tile(&mut self, space: &mut TreeSpace<W>, id: &W::Id) -> RemovedTile<W> {
        let idx = self.idx_of(space, id).unwrap();
        self.remove_tile_from_container(space, idx, id)
    }

    pub(super) fn unfloat_container(
        &mut self,
        space: &mut TreeSpace<W>,
        id: &W::Id,
        reference: Option<&InactiveTilingReference>,
        as_workspace: bool,
        focus: bool,
    ) -> bool {
        let Some(idx) = self.idx_of(space, id) else {
            return false;
        };
        let root = self.containers[idx].root;
        if space
            .tree()
            .fullscreen_key()
            .is_some_and(|key| space.tree().is_descendant(key, root))
        {
            space.tree_mut().set_fullscreen_key(None);
        }

        if let Some(resize) = &self.interactive_resize {
            if self.idx_of(space, &resize.window) == Some(idx) {
                self.interactive_resize = None;
            }
        }

        for tile in space.tree_mut().tiles_in_branch_mut(root) {
            Self::store_floating_size_for_restore(tile);
        }
        let changed = if as_workspace {
            space.tree_mut().unfloat_as_workspace(root, focus)
        } else if let Some(reference) = reference {
            space
                .tree_mut()
                .unfloat_with_tiling_reference(root, reference, focus)
        } else {
            space.tree_mut().unfloat_into_workspace(root, focus)
        };
        if !changed {
            return false;
        }
        self.containers.remove(idx);
        space.tree_mut().layout();

        true
    }

    pub(super) fn unfloat_window(
        &mut self,
        space: &mut TreeSpace<W>,
        id: &W::Id,
        reference: Option<&InactiveTilingReference>,
        focus: bool,
    ) -> bool {
        let Some(idx) = self.idx_of(space, id) else {
            return false;
        };
        let Some(key) = space.tree_mut().window_key(id) else {
            return false;
        };
        if space.tree().window_owns_fullscreen(id) {
            // Clear the workspace pointer before the node crosses branches so the arrange below
            // computes its tiled geometry, not another fullscreen frame that no longer has an
            // owner after this operation.
            space.tree_mut().set_fullscreen_key(None);
        }
        let pos = space
            .tree()
            .floating_position(self.containers[idx].root)
            .expect("every floating stack entry must name a floating root");
        if let Some(tile) = space.tree_mut().get_tile_mut(key) {
            Self::store_floating_size_for_restore(tile);
            tile.floating_pos = Some(pos);
            tile.set_scratchpad(false);
        }
        let Some(group_empty) = space.tree_mut().unfloat_node(key, reference, focus) else {
            return false;
        };
        if group_empty {
            self.containers.remove(idx);
        }
        space.tree_mut().layout();
        if self
            .interactive_resize
            .as_ref()
            .is_some_and(|resize| &resize.window == id)
        {
            self.interactive_resize = None;
        }
        true
    }

    pub(super) fn active_container_is_workspace_floated(&self, space: &TreeSpace<W>) -> bool {
        self.active_window_id(space)
            .as_ref()
            .and_then(|id| self.idx_of(space, id))
            .is_some_and(|idx| self.containers[idx].kind.is_workspace_wrapper())
    }

    fn remove_tile_from_container(
        &mut self,
        space: &mut TreeSpace<W>,
        idx: usize,
        id: &W::Id,
    ) -> RemovedTile<W> {
        let container_pos = space
            .tree()
            .floating_position(self.containers[idx].root)
            .expect("every floating stack entry must name a floating root");
        let container_id = self.containers[idx].id;
        let insert_hint = space.tree().insert_parent_info_for_window(id);
        let mut tile = space
            .tree_mut()
            .remove_window(id)
            .expect("window must exist in floating container");

        // Stop interactive resize.
        if let Some(resize) = &self.interactive_resize {
            if tile.window().id() == &resize.window {
                self.interactive_resize = None;
            }
        }

        if space
            .tree()
            .window_count_in_branch(self.containers[idx].root)
            == 0
        {
            let root = self.containers.remove(idx).root;
            space.tree_mut().forget_floating_root(root);
        }

        Self::store_floating_size_for_restore(&mut tile);
        // Store the floating position.
        tile.floating_pos = Some(container_pos);
        tile.floating_reinsert_hint = insert_hint.map(|info| (container_id, info));

        let width = ColumnWidth::Fixed(tile.tile_expected_or_current_size().w as i32);
        RemovedTile {
            tile,
            width,
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
        space: &mut TreeSpace<W>,
        renderer: &mut GlesRenderer,
        id: &W::Id,
        blocker: TransactionBlocker,
    ) {
        let (tile, tile_pos) = self
            .tiles_with_render_positions_mut(space, false)
            .find(|(tile, _)| tile.window().id() == id)
            .unwrap();

        let Some(snapshot) = tile.take_unmap_snapshot() else {
            return;
        };

        let tile_size = tile.tile_size();

        self.start_close_animation_for_tile(
            space, renderer, snapshot, tile_size, tile_pos, blocker,
        );
    }

    pub fn activate_window_without_raising(
        &mut self,
        space: &mut TreeSpace<W>,
        id: &W::Id,
    ) -> bool {
        let Some(_idx) = self.idx_of(space, id) else {
            return false;
        };

        space.tree_mut().clear_selection();
        let _ = space.tree_mut().focus_window_by_id(id);
        true
    }

    pub fn activate_window(&mut self, space: &mut TreeSpace<W>, id: &W::Id) -> bool {
        let Some(idx) = self.idx_of(space, id) else {
            return false;
        };

        self.raise_container(idx, 0);
        self.bring_up_descendants_of(space, 0);
        space.tree_mut().clear_selection();
        let _ = space.tree_mut().focus_window_by_id(id);

        true
    }

    fn raise_container(&mut self, from_idx: usize, to_idx: usize) {
        assert!(to_idx <= from_idx);

        let container = self.containers.remove(from_idx);
        self.containers.insert(to_idx, container);
    }

    pub fn start_close_animation_for_tile(
        &mut self,
        space: &TreeSpace<W>,
        renderer: &mut GlesRenderer,
        snapshot: TileRenderSnapshot,
        tile_size: Size<f64, Logical>,
        tile_pos: Point<f64, Logical>,
        blocker: TransactionBlocker,
    ) {
        let anim = Animation::new(
            space.clock().clone(),
            0.,
            1.,
            0.,
            space.options().animations.window_close.anim,
        );

        let scale = Scale::from(space.scale());
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

    fn resolve_target_id(&self, space: &TreeSpace<W>, id: Option<&W::Id>) -> Option<W::Id> {
        id.cloned().or_else(|| self.active_window_id(space))
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
        space: &mut TreeSpace<W>,
        id: Option<&W::Id>,
        forwards: bool,
    ) {
        let Some(id) = self.resolve_target_id(space, id) else {
            return;
        };
        let available_size = space.working_area().size.w;
        let presets = space.options().layout.preset_column_widths.clone();

        let Some(tile) = self.tile_at_mut(space, &id) else {
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
        self.set_window_width(space, Some(&id), SizeChange::from(preset), true);

        if let Some(tile) = self.tile_at_mut(space, &id) {
            tile.floating_preset_width_idx = Some(preset_idx);
        }

        self.interactive_resize_end(Some(&id));
    }

    pub fn start_open_animation(&self, space: &mut TreeSpace<W>, id: &W::Id) -> bool {
        if let Some(tile) = self.tile_at_mut(space, id) {
            tile.start_open_animation();
            true
        } else {
            false
        }
    }

    pub fn toggle_window_height(
        &mut self,
        space: &mut TreeSpace<W>,
        id: Option<&W::Id>,
        forwards: bool,
    ) {
        let Some(id) = self.resolve_target_id(space, id) else {
            return;
        };
        let available_size = space.working_area().size.h;
        let presets = space.options().layout.preset_window_heights.clone();

        let Some(tile) = self.tile_at_mut(space, &id) else {
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
        self.set_window_height(space, Some(&id), SizeChange::from(preset), true);

        if let Some(tile) = self.tile_at_mut(space, &id) {
            tile.floating_preset_height_idx = Some(preset_idx);
        }

        self.interactive_resize_end(Some(&id));
    }

    fn resize_container_dimension(
        &mut self,
        space: &mut TreeSpace<W>,
        idx: usize,
        change: SizeChange,
        axis: ResizeAxis,
        anchor: FloatingResizeAnchor,
    ) {
        let is_width = axis == ResizeAxis::Horizontal;
        let available = if is_width {
            space.working_area().size.w
        } else {
            space.working_area().size.h
        };
        let current_area = self.container_area(space, idx);
        let current = if is_width {
            current_area.size.w
        } else {
            current_area.size.h
        };

        const MAX_PX: f64 = 100000.;
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
        .clamp(1., MAX_PX);

        let size = if is_width {
            Size::from((new_size, current_area.size.h))
        } else {
            Size::from((current_area.size.w, new_size))
        };
        self.set_container_size(space, idx, size);
        if anchor == FloatingResizeAnchor::Center {
            let centered_pos = Point::from((
                current_area.loc.x + (current_area.size.w - size.w) / 2.,
                current_area.loc.y + (current_area.size.h - size.h) / 2.,
            ));
            self.set_container_logical_pos(space, idx, centered_pos);
        }
        let root = self.containers[idx].root;
        space.tree_mut().layout_branch(root);
    }

    fn resize_container_around_center(
        &mut self,
        space: &mut TreeSpace<W>,
        idx: usize,
        change: SizeChange,
        axis: ResizeAxis,
    ) {
        self.resize_container_dimension(space, idx, change, axis, FloatingResizeAnchor::Center);
    }

    pub fn set_window_width(
        &mut self,
        space: &mut TreeSpace<W>,
        id: Option<&W::Id>,
        change: SizeChange,
        animate: bool,
    ) {
        let active = self.active_window_id(space);
        let Some(target_id) = id.or(active.as_ref()) else {
            return;
        };
        let idx = self.idx_of(space, target_id).unwrap();
        let selection_is_container = id.is_none() && self.selected_is_container_in(space, idx);
        if selection_is_container {
            self.resize_container_around_center(space, idx, change, ResizeAxis::Horizontal);
            return;
        }

        let key = if let Some(id) = id {
            match space.tree_mut().window_key(id) {
                Some(key) => key,
                None => return,
            }
        } else {
            match self.selected_key_in(space, idx) {
                Some(key) => key,
                None => return,
            }
        };

        if let Some(tile) = space.tree_mut().get_tile_mut(key) {
            tile.floating_preset_width_idx = None;
        }

        let Some((parent_key, child_idx, available, child_count, _)) =
            space.container_metrics_for(key, Layout::SplitH)
        else {
            self.resize_container_around_center(space, idx, change, ResizeAxis::Horizontal);
            return;
        };
        if child_count <= 1 {
            self.resize_container_around_center(space, idx, change, ResizeAxis::Horizontal);
            return;
        }

        let current_percent = space
            .tree_mut()
            .child_percent(parent_key, child_idx)
            .unwrap_or(1.0);
        let percent = percent_from_size_change(
            current_percent,
            available,
            || space.ppt_reference(key, Layout::SplitH),
            change,
        );

        if space
            .tree_mut()
            .set_child_percent(parent_key, child_idx, Layout::SplitH, percent)
        {
            let _ = animate;
            space.tree_mut().layout_branch(self.containers[idx].root);
        }
    }

    /// Apply a keyboard/IPC resize to a floating target.
    ///
    /// Axis requests change the size directly. Edge requests reuse the same geometry operation as
    /// an edge drag, including anchoring the opposite edge, but remain a one-shot command rather
    /// than becoming interactive state owned by the input layer.
    pub fn resize_window(
        &mut self,
        space: &mut TreeSpace<W>,
        id: Option<&W::Id>,
        request: ResizeRequest,
    ) {
        match request {
            ResizeRequest::Axis {
                axis: ResizeAxis::Horizontal,
                change,
            } => self.set_window_width(space, id, change, true),
            ResizeRequest::Axis {
                axis: ResizeAxis::Vertical,
                change,
            } => self.set_window_height(space, id, change, true),
            ResizeRequest::Edge { direction, amount } => {
                self.resize_window_edge(space, id, direction, amount)
            }
        }
    }

    fn resize_window_edge(
        &mut self,
        space: &mut TreeSpace<W>,
        id: Option<&W::Id>,
        direction: Direction,
        amount: i32,
    ) {
        let Some(id) = self.resolve_target_id(space, id) else {
            return;
        };
        let amount = f64::from(amount);
        let (edge, delta) = match direction {
            Direction::Left => (ResizeEdge::LEFT, Point::from((-amount, 0.))),
            Direction::Right => (ResizeEdge::RIGHT, Point::from((amount, 0.))),
            Direction::Up => (ResizeEdge::TOP, Point::from((0., -amount))),
            Direction::Down => (ResizeEdge::BOTTOM, Point::from((0., amount))),
        };

        if self.interactive_resize_begin(space, id.clone(), edge) {
            self.interactive_resize_update(space, &id, delta);
            self.interactive_resize_end(Some(&id));
        }
    }

    pub fn set_window_height(
        &mut self,
        space: &mut TreeSpace<W>,
        id: Option<&W::Id>,
        change: SizeChange,
        animate: bool,
    ) {
        let active = self.active_window_id(space);
        let Some(target_id) = id.or(active.as_ref()) else {
            return;
        };
        let idx = self.idx_of(space, target_id).unwrap();
        let selection_is_container = id.is_none() && self.selected_is_container_in(space, idx);
        if selection_is_container {
            self.resize_container_around_center(space, idx, change, ResizeAxis::Vertical);
            return;
        }

        let key = if let Some(id) = id {
            match space.tree_mut().window_key(id) {
                Some(key) => key,
                None => return,
            }
        } else {
            match self.selected_key_in(space, idx) {
                Some(key) => key,
                None => return,
            }
        };

        if let Some(tile) = space.tree_mut().get_tile_mut(key) {
            tile.floating_preset_height_idx = None;
        }

        let Some((parent_key, child_idx, available, child_count, _)) =
            space.container_metrics_for(key, Layout::SplitV)
        else {
            self.resize_container_around_center(space, idx, change, ResizeAxis::Vertical);
            return;
        };
        if child_count <= 1 {
            self.resize_container_around_center(space, idx, change, ResizeAxis::Vertical);
            return;
        }

        let current_percent = space
            .tree_mut()
            .child_percent(parent_key, child_idx)
            .unwrap_or(1.0);
        let percent = percent_from_size_change(
            current_percent,
            available,
            || space.ppt_reference(key, Layout::SplitV),
            change,
        );

        if space
            .tree_mut()
            .set_child_percent(parent_key, child_idx, Layout::SplitV, percent)
        {
            let _ = animate;
            space.tree_mut().layout_branch(self.containers[idx].root);
        }
    }

    fn focus_directional(
        &mut self,
        space: &mut TreeSpace<W>,
        distance: impl Fn(Point<f64, Logical>, Point<f64, Logical>) -> f64,
    ) -> bool {
        let Some(active_id) = self.active_window_id(space) else {
            return false;
        };
        let (active_tile, active_pos) = match self
            .tiles_with_offsets_visible(space)
            .find(|(tile, _)| tile.window().id() == &active_id)
        {
            Some(value) => value,
            None => return false,
        };
        let center = active_pos + active_tile.tile_size().downscale(2.);

        let result = self
            .tiles_with_offsets_visible(space)
            .filter(|(tile, _)| tile.window().id() != &active_id)
            .map(|(tile, pos)| {
                let other_center = pos + tile.tile_size().downscale(2.);
                (tile, distance(center, other_center))
            })
            .filter(|(_, dist)| *dist > 0.)
            .min_by(|(_, dist_a), (_, dist_b)| f64::total_cmp(dist_a, dist_b));
        if let Some((tile, _)) = result {
            let id = tile.window().id().clone();
            self.activate_window(space, &id);
            true
        } else {
            false
        }
    }

    fn focus_within_active_container(
        &mut self,
        space: &mut TreeSpace<W>,
        direction: Direction,
        allow_wrap: bool,
    ) -> bool {
        let Some(idx) = self.active_container_idx(space) else {
            return false;
        };
        let allow_wrap = allow_wrap && !self.selected_is_container_in(space, idx);
        let root = self.containers[idx].root;
        let moved = space
            .tree_mut()
            .focus_in_direction_in_branch(root, direction, allow_wrap);
        if moved {
            return true;
        }

        false
    }

    fn focus_in_stack_order(&mut self, space: &mut TreeSpace<W>, delta: isize) -> bool {
        if self.containers.len() <= 1 {
            return false;
        }

        let Some(idx) = self.active_container_idx(space) else {
            return false;
        };
        let len = self.containers.len() as isize;
        let target_idx = (idx as isize + delta).rem_euclid(len) as usize;
        if target_idx == idx {
            return false;
        }

        let root = self.containers[target_idx].root;
        let Some(id) = space
            .tree_mut()
            .focused_window_in_branch(root)
            .map(|win| win.id().clone())
            .or_else(|| {
                space
                    .tree_mut()
                    .windows_in_branch(root)
                    .into_iter()
                    .next()
                    .map(|win| win.id().clone())
            })
        else {
            return false;
        };

        self.activate_window(space, &id);
        true
    }

    fn focus_in_stable_container_order(
        &mut self,
        space: &mut TreeSpace<W>,
        descending: bool,
    ) -> bool {
        if self.containers.len() <= 1 {
            return false;
        }

        let Some(active_idx) = self.active_container_idx(space) else {
            return false;
        };

        let mut ordered: Vec<_> = self
            .containers
            .iter()
            .enumerate()
            .map(|(idx, container)| (idx, container.id))
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

        let root = self.containers[target_idx].root;
        let Some(id) = space
            .tree_mut()
            .focused_window_in_branch(root)
            .map(|win| win.id().clone())
            .or_else(|| {
                space
                    .tree_mut()
                    .windows_in_branch(root)
                    .into_iter()
                    .next()
                    .map(|win| win.id().clone())
            })
        else {
            return false;
        };

        self.activate_window(space, &id);
        true
    }

    fn should_cycle_top_level_stable_order(&self, space: &TreeSpace<W>) -> bool {
        self.containers.len() > 1
            && self
                .active_container_idx(space)
                .is_some_and(|idx| space.branch_root_is_implicit(self.containers[idx].root))
            && self
                .containers
                .iter()
                .all(|container| space.branch_root_is_implicit(container.root))
    }

    pub fn focus_left(&mut self, space: &mut TreeSpace<W>) -> bool {
        if self.focus_within_active_container(space, Direction::Left, true) {
            return true;
        }
        if self.should_cycle_top_level_stable_order(space) {
            return self.focus_in_stable_container_order(space, true);
        }
        self.focus_in_stack_order(space, 1)
            || self.focus_directional(space, |focus, other| focus.x - other.x)
    }

    pub fn focus_left_no_wrap(&mut self, space: &mut TreeSpace<W>) -> bool {
        if self.focus_within_active_container(space, Direction::Left, false) {
            return true;
        }
        self.focus_directional(space, |focus, other| focus.x - other.x)
    }

    pub fn focus_window_by_id(&mut self, space: &mut TreeSpace<W>, id: &W::Id) -> bool {
        let Some(_idx) = self.idx_of(space, id) else {
            return false;
        };

        space.tree_mut().clear_selection();
        let _ = space.tree_mut().focus_window_by_id(id);
        true
    }

    pub fn focus_right(&mut self, space: &mut TreeSpace<W>) -> bool {
        if self.focus_within_active_container(space, Direction::Right, true) {
            return true;
        }
        if self.should_cycle_top_level_stable_order(space) {
            return self.focus_in_stable_container_order(space, false);
        }
        self.focus_in_stack_order(space, -1)
            || self.focus_directional(space, |focus, other| other.x - focus.x)
    }

    pub fn focus_right_no_wrap(&mut self, space: &mut TreeSpace<W>) -> bool {
        if self.focus_within_active_container(space, Direction::Right, false) {
            return true;
        }
        self.focus_directional(space, |focus, other| other.x - focus.x)
    }

    pub fn focus_up(&mut self, space: &mut TreeSpace<W>) -> bool {
        if self.focus_within_active_container(space, Direction::Up, true) {
            return true;
        }
        self.focus_directional(space, |focus, other| focus.y - other.y)
    }

    pub fn focus_up_no_wrap(&mut self, space: &mut TreeSpace<W>) -> bool {
        if self.focus_within_active_container(space, Direction::Up, false) {
            return true;
        }
        self.focus_directional(space, |focus, other| focus.y - other.y)
    }

    pub fn focus_down(&mut self, space: &mut TreeSpace<W>) -> bool {
        if self.focus_within_active_container(space, Direction::Down, true) {
            return true;
        }
        self.focus_directional(space, |focus, other| other.y - focus.y)
    }

    pub fn focus_down_no_wrap(&mut self, space: &mut TreeSpace<W>) -> bool {
        if self.focus_within_active_container(space, Direction::Down, false) {
            return true;
        }
        self.focus_directional(space, |focus, other| other.y - focus.y)
    }

    pub fn focus_leftmost(&mut self, space: &mut TreeSpace<W>) {
        let result = self
            .tiles_with_offsets_visible(space)
            .min_by(|(_, pos_a), (_, pos_b)| f64::total_cmp(&pos_a.x, &pos_b.x));
        if let Some((tile, _)) = result {
            let id = tile.window().id().clone();
            self.activate_window(space, &id);
        }
    }

    pub fn focus_rightmost(&mut self, space: &mut TreeSpace<W>) {
        let result = self
            .tiles_with_offsets_visible(space)
            .max_by(|(_, pos_a), (_, pos_b)| f64::total_cmp(&pos_a.x, &pos_b.x));
        if let Some((tile, _)) = result {
            let id = tile.window().id().clone();
            self.activate_window(space, &id);
        }
    }

    pub fn focus_topmost(&mut self, space: &mut TreeSpace<W>) {
        let result = self
            .tiles_with_offsets_visible(space)
            .min_by(|(_, pos_a), (_, pos_b)| f64::total_cmp(&pos_a.y, &pos_b.y));
        if let Some((tile, _)) = result {
            let id = tile.window().id().clone();
            self.activate_window(space, &id);
        }
    }

    pub fn focus_bottommost(&mut self, space: &mut TreeSpace<W>) {
        let result = self
            .tiles_with_offsets_visible(space)
            .max_by(|(_, pos_a), (_, pos_b)| f64::total_cmp(&pos_a.y, &pos_b.y));
        if let Some((tile, _)) = result {
            let id = tile.window().id().clone();
            self.activate_window(space, &id);
        }
    }

    pub(super) fn focus_parent(&mut self, space: &mut TreeSpace<W>) -> bool {
        let Some(idx) = self.active_container_idx(space) else {
            return false;
        };
        let root = self.containers[idx].root;
        if space.handle_focus_parent_in_branch_fullscreen_scope(root) {
            return true;
        }

        // The floating root is an ordinary sway container. Once it holds focus, the next
        // parent is the workspace, which Workspace represents outside ContainerTree; leave
        // the root selected so command routing still knows which floating group was raised.
        if space.tree_mut().selected_container_key() == Some(root) {
            return space.tree_mut().select_parent();
        }

        // One step, not a walk: `select_parent_in` stops at the branch root, so there is
        // never a second ancestor to consider. This was written as a loop that every path
        // left on its first pass.
        if !space.tree_mut().select_parent_in(root) {
            return false;
        }

        let Some(key) = space.tree_mut().selected_container_key() else {
            return false;
        };
        let meaningful = space
            .tree_mut()
            .container_is_meaningful_parent(key)
            .unwrap_or(false);
        if key != root || meaningful {
            return true;
        }

        // The root around a lone floating view exists only because tiri needs a node for the
        // entry in ws->floating. sway does not expose an extra focus-parent stop for it.
        space.tree_mut().select_parent()
    }

    pub(super) fn focus_child_from_workspace(&mut self, space: &mut TreeSpace<W>) -> bool {
        let Some(idx) = self.active_container_idx(space) else {
            return false;
        };
        let root = self.containers[idx].root;
        space.tree_mut().select_container(root) && space.tree_mut().select_child()
    }

    pub fn focus_child(&mut self, space: &mut TreeSpace<W>) -> bool {
        let Some(idx) = self.active_container_idx(space) else {
            return false;
        };
        let root = self.containers[idx].root;
        space
            .tree_mut()
            .selected_container_key()
            .is_some_and(|key| space.tree_mut().is_descendant(key, root))
            && space.tree_mut().select_child()
    }

    fn consume_or_expel_window(
        &mut self,
        space: &mut TreeSpace<W>,
        window: Option<&W::Id>,
        direction: Direction,
    ) {
        if let Some(id) = window {
            if !self.activate_window(space, id) {
                return;
            }
        }

        let Some(idx) = self.active_container_idx(space) else {
            return;
        };

        if self.move_tree_command_target(space, idx, direction) {
            return;
        }

        self.split_container(space, idx, Layout::SplitV);
    }

    pub fn consume_or_expel_window_left(
        &mut self,
        space: &mut TreeSpace<W>,
        window: Option<&W::Id>,
    ) {
        self.consume_or_expel_window(space, window, Direction::Left);
    }

    pub fn consume_or_expel_window_right(
        &mut self,
        space: &mut TreeSpace<W>,
        window: Option<&W::Id>,
    ) {
        self.consume_or_expel_window(space, window, Direction::Right);
    }

    pub fn consume_into_column(&mut self, space: &mut TreeSpace<W>) {
        let Some(idx) = self.active_container_idx(space) else {
            return;
        };
        self.split_container(space, idx, Layout::SplitV);
    }

    pub fn expel_from_column(&mut self, space: &mut TreeSpace<W>) {
        let Some(idx) = self.active_container_idx(space) else {
            return;
        };
        self.split_container(space, idx, Layout::SplitH);
    }

    fn move_tree_command_target(
        &mut self,
        space: &mut TreeSpace<W>,
        idx: usize,
        direction: Direction,
    ) -> bool {
        space.move_in_branch(self.containers[idx].root, direction)
    }

    pub fn set_column_display(&mut self, space: &mut TreeSpace<W>, display: ColumnDisplay) {
        let target_layout = match display {
            ColumnDisplay::Normal => Layout::SplitV,
            ColumnDisplay::Tabbed => Layout::Tabbed,
        };

        let Some(idx) = self.active_container_idx(space) else {
            return;
        };
        space.set_layout_in_branch(self.containers[idx].root, target_layout);
    }

    pub fn toggle_column_tabbed_display(&mut self, space: &mut TreeSpace<W>) {
        let Some(idx) = self.active_container_idx(space) else {
            return;
        };
        let target = match space.selection_layout_in(self.containers[idx].root) {
            Some(Layout::Tabbed) => Layout::SplitV,
            _ => Layout::Tabbed,
        };
        space.set_layout_in_branch(self.containers[idx].root, target);
    }

    pub fn split_horizontal(&mut self, space: &mut TreeSpace<W>) {
        let Some(idx) = self.active_container_idx(space) else {
            return;
        };
        self.split_container(space, idx, Layout::SplitH);
    }

    pub fn split_vertical(&mut self, space: &mut TreeSpace<W>) {
        let Some(idx) = self.active_container_idx(space) else {
            return;
        };
        self.split_container(space, idx, Layout::SplitV);
    }

    pub fn split_none(&mut self, space: &mut TreeSpace<W>) {
        let Some(idx) = self.active_container_idx(space) else {
            return;
        };
        let Some(root) = space.unsplit_in_branch(self.containers[idx].root) else {
            return;
        };
        self.containers[idx].root = root;
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
    fn root_kind(&self, space: &TreeSpace<W>, idx: usize) -> FloatingRootKind {
        let stored = self.containers[idx].kind;
        if stored != FloatingRootKind::ImplicitWindowGroup {
            return stored;
        }
        if space
            .tree()
            .branch_container(self.containers[idx].root)
            .is_some_and(|root| root.is_user_container())
        {
            FloatingRootKind::FloatedContainer
        } else {
            stored
        }
    }

    fn split_container(&mut self, space: &mut TreeSpace<W>, idx: usize, layout: Layout) -> bool {
        let root = self.containers[idx].root;
        space.split_in_branch(root, layout)
    }

    pub fn set_layout_mode(&mut self, space: &mut TreeSpace<W>, layout: Layout) {
        let Some(idx) = self.active_container_idx(space) else {
            return;
        };
        space.set_layout_in_branch(self.containers[idx].root, layout);
    }

    pub fn toggle_split_layout(&mut self, space: &mut TreeSpace<W>) {
        let Some(idx) = self.active_container_idx(space) else {
            return;
        };
        space.toggle_split_in_branch(self.containers[idx].root);
    }

    pub fn toggle_layout_all(&mut self, space: &mut TreeSpace<W>) {
        let Some(idx) = self.active_container_idx(space) else {
            return;
        };
        space.toggle_layout_all_in_branch(self.containers[idx].root);
    }

    pub fn set_default_layout(&mut self, space: &mut TreeSpace<W>) {
        let Some(idx) = self.active_container_idx(space) else {
            return;
        };
        space.set_default_layout_in_branch(self.containers[idx].root);
    }

    pub(super) fn toggle_layout_cycle(
        &mut self,
        space: &mut TreeSpace<W>,
        cycle: &[LayoutCycleEntry],
    ) {
        let Some(idx) = self.active_container_idx(space) else {
            return;
        };
        space.toggle_layout_cycle_in_branch(self.containers[idx].root, cycle);
    }

    fn move_container_to(
        &mut self,
        space: &mut TreeSpace<W>,
        idx: usize,
        new_pos: Point<f64, Logical>,
        animate: bool,
    ) {
        if animate {
            self.move_container_and_animate(space, idx, new_pos);
        } else {
            self.set_container_logical_pos(space, idx, new_pos);
        }

        let root = self.containers[idx].root;
        space.tree_mut().layout_branch(root);

        self.interactive_resize_end(None);
    }

    fn move_by(&mut self, space: &mut TreeSpace<W>, amount: Point<f64, Logical>) {
        let Some(active_id) = self.active_window_id(space) else {
            return;
        };
        if self.is_fullscreen(space, &active_id) {
            return;
        }
        let idx = self.idx_of(space, &active_id).unwrap();

        let new_pos = self.container_area(space, idx).loc + amount;
        self.move_container_to(space, idx, new_pos, true)
    }

    pub fn move_left(&mut self, space: &mut TreeSpace<W>) {
        self.move_by(space, Point::from((-DIRECTIONAL_MOVE_PX, 0.)));
    }

    pub fn move_right(&mut self, space: &mut TreeSpace<W>) {
        self.move_by(space, Point::from((DIRECTIONAL_MOVE_PX, 0.)));
    }

    pub fn move_up(&mut self, space: &mut TreeSpace<W>) {
        self.move_by(space, Point::from((0., -DIRECTIONAL_MOVE_PX)));
    }

    pub fn move_down(&mut self, space: &mut TreeSpace<W>) {
        self.move_by(space, Point::from((0., DIRECTIONAL_MOVE_PX)));
    }

    pub fn move_window(
        &mut self,
        space: &mut TreeSpace<W>,
        id: Option<&W::Id>,
        x: PositionChange,
        y: PositionChange,
        animate: bool,
    ) {
        let Some(id) = self.resolve_target_id(space, id) else {
            return;
        };
        if self.is_fullscreen(space, &id) {
            return;
        }
        let idx = self.idx_of(space, &id).unwrap();

        let mut pos = self.container_area(space, idx).loc;

        let available_width = space.working_area().size.w;
        let available_height = space.working_area().size.h;
        let working_area_loc = space.working_area().loc;

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

        self.move_container_to(space, idx, pos, animate);
    }

    pub fn center_window(&mut self, space: &mut TreeSpace<W>, id: Option<&W::Id>) {
        let Some(id) = self.resolve_target_id(space, id) else {
            return;
        };
        if self.is_fullscreen(space, &id) {
            return;
        }
        let idx = self.idx_of(space, &id).unwrap();

        let new_pos = center_preferring_top_left_in_area(
            space.working_area(),
            self.container_area(space, idx).size,
        );
        self.move_container_to(space, idx, new_pos, true);
    }

    pub fn descendants_added(&mut self, space: &TreeSpace<W>, id: &W::Id) -> bool {
        let Some(idx) = self.idx_of(space, id) else {
            return false;
        };

        self.bring_up_descendants_of(space, idx);
        true
    }

    pub fn update_window(
        &mut self,
        space: &mut TreeSpace<W>,
        id: &W::Id,
        serial: Option<Serial>,
    ) -> bool {
        let Some(container_idx) = self.idx_of(space, id) else {
            return false;
        };

        {
            let Some(key) = space.tree_mut().window_key(id) else {
                return false;
            };
            let Some(tile) = space.tree_mut().get_tile_mut(key) else {
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

        let root = self.containers[container_idx].root;
        space.tree_mut().layout_branch(root);

        if space.tree().window_count_in_branch(root) == 1 {
            let Some(key) = space.tree_mut().window_key(id) else {
                return true;
            };
            let Some(tile) = space.tree_mut().get_tile(key) else {
                return true;
            };
            // Fullscreen is a temporary output-sized commit, not a new floating resize base.
            // Keeping it would make unfullscreen restore the output size instead of the last
            // ordinary floating size.
            if tile.window().pending_sizing_mode().is_normal() {
                let tile_size = tile.tile_size();
                space
                    .tree_mut()
                    .record_floating_resize_base(root, tile_size);
            }
        }

        true
    }

    fn render_elements<R: NiriRenderer>(
        &self,
        space: &TreeSpace<W>,
        mut ctx: RenderCtx<R>,
        xray_pos: XrayPos,
        view_rect: Rectangle<f64, Logical>,
        focus_ring: bool,
    ) -> Vec<FloatingSpaceRenderElement<R>> {
        let tree = space.tree();
        let tile_count = self.tiles(space).count();
        let estimated_capacity = tile_count * 4 + self.closing_windows.len() + tile_count / 2;
        let mut elements = Vec::with_capacity(estimated_capacity);
        let scale = Scale::from(space.scale());

        // Draw the closing windows on top of the other windows.
        //
        // FIXME: I guess this should rather preserve the stacking order when the window is closed.
        for closing in self.closing_windows.iter().rev() {
            let elem = closing.render(ctx.as_gles(), view_rect, scale);
            elements.push(elem.into());
        }

        let active = self.active_window_id(space);
        let fullscreen_key = self.fullscreen_key(space);
        let fullscreen_id = self.fullscreen_window_id(space).cloned();
        let selection_is_container = self
            .active_container_idx(space)
            .is_some_and(|idx| self.selected_is_container_in(space, idx));

        // Like tiling, push container selection before the regular window
        // contents so it stays visually on top after the global reverse-order
        // composition pass in the renderer.
        if (focus_ring || space.side_is_active(true))
            && selection_is_container
            && fullscreen_id.is_none()
        {
            if let Some(idx) = self.active_container_idx(space) {
                if let Some((_, local_rect, _)) = self
                    .selected_key_in(space, idx)
                    .and_then(|key| tree.container_info(key))
                {
                    let rect = local_rect;
                    render_container_selection(
                        ctx.renderer,
                        rect,
                        view_rect,
                        space.scale(),
                        space.side_is_active(true),
                        space.options().layout.focus_ring,
                        space.options().layout.border,
                        ContainerSelectionStyle::Floating,
                        &mut |elem| {
                            elements.push(FloatingSpaceRenderElement::ContainerSelection(elem))
                        },
                    );
                }
            }
        }

        if !space.options().layout.tab_bar.off && fullscreen_id.is_none() {
            let mut cache = space.tab_bar_cache_mut();
            let gles = ctx.renderer.as_gles_renderer();
            let tab_bar_config = space.options().layout.tab_bar.clone();
            let is_active_workspace = space.side_is_active(true);
            let target = ctx.target;

            let roots: Vec<NodeKey> = if let Some(scope) = fullscreen_key {
                vec![scope]
            } else {
                self.containers
                    .iter()
                    .map(|container| container.root)
                    .collect()
            };
            for root in roots {
                let gap = space.branch_gap(root);
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
                        space.scale(),
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
                            space.scale(),
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
            if let Some(tile) = self.tiles(space).find(|t| t.window().id() == fullscreen_id) {
                let is_focused = space.side_is_active(true);
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
            for (tile, tile_pos) in self.tiles_with_render_positions(space) {
                if fullscreen_key.is_some_and(|scope| !tree.is_descendant(tile.node_key(), scope)) {
                    continue;
                }
                // Skip tiles entirely outside the viewport (culling)
                let tile_rect = Rectangle::new(tile_pos, tile.tile_size());
                if !tile_rect.overlaps(view_rect) {
                    continue;
                }

                let is_focused = space.side_is_active(true)
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

    pub fn render<R: NiriRenderer>(
        &self,
        space: &TreeSpace<W>,
        ctx: RenderCtx<R>,
        xray_pos: XrayPos,
        view_rect: Rectangle<f64, Logical>,
        focus_ring: bool,
        push: &mut dyn FnMut(FloatingSpaceRenderElement<R>),
    ) {
        for elem in self.render_elements(space, ctx, xray_pos, view_rect, focus_ring) {
            push(elem);
        }
    }

    pub fn interactive_resize_begin(
        &mut self,
        space: &TreeSpace<W>,
        window: W::Id,
        edges: ResizeEdge,
    ) -> bool {
        let tree = space.tree();
        if self.interactive_resize.is_some() {
            return false;
        }

        let Some(idx) = self.idx_of(space, &window) else {
            return false;
        };

        let container = &self.containers[idx];
        let Some(key) = tree.window_key(&window) else {
            return false;
        };
        let Some(tile) = tree.get_tile(key) else {
            return false;
        };

        let original_window_size = tile.window_size();
        let container_area = tree
            .floating_container_area(container.root)
            .expect("every floating stack entry must name a floating root");
        let original_window_pos = container_area.loc;
        let original_container_size = container_area.size;
        let resize_container_edges =
            super::tree_space::branch_display_layouts(space.tree(), container.root)
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
        space: &mut TreeSpace<W>,
        window: &W::Id,
        delta: Point<f64, Logical>,
    ) -> bool {
        let Some(idx) = self.idx_of(space, window) else {
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
            let Some(tile) = space
                .tree_mut()
                .window_key(window)
                .and_then(|key| space.tree_mut().get_tile(key))
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
                    space,
                    idx,
                    SizeChange::SetFixed(target_width),
                    ResizeAxis::Horizontal,
                    FloatingResizeAnchor::KeepOrigin,
                );
            } else {
                self.set_window_width(
                    space,
                    Some(window),
                    SizeChange::SetFixed(target_width),
                    false,
                );
            }
        }

        if edges.intersects(ResizeEdge::TOP_BOTTOM) {
            if resize_container_v {
                self.resize_container_dimension(
                    space,
                    idx,
                    SizeChange::SetFixed(target_height),
                    ResizeAxis::Vertical,
                    FloatingResizeAnchor::KeepOrigin,
                );
            } else {
                self.set_window_height(
                    space,
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
                self.set_container_logical_pos(space, idx, original_pos + move_pos);
                let root = self.containers[idx].root;
                space.tree_mut().layout_branch(root);
            }
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

    pub fn refresh(&mut self, space: &mut TreeSpace<W>, is_active: bool, is_focused: bool) {
        let _span = tracy_client::span!("FloatingSpace::refresh");
        let active = self.active_window_id(space);
        let deactivate_unfocused = space.options().deactivate_unfocused_windows;
        let disable_resize_throttling = space.options().disable_resize_throttling;
        let border_base = space.options().layout.border;
        let working_area_size = space.working_area().size;
        let resize_target = self.interactive_resize.as_ref().and_then(|resize| {
            let idx = self.idx_of(space, &resize.window)?;
            let mut ids = Vec::new();
            for tile in space.tree_mut().tiles_in_branch(self.containers[idx].root) {
                ids.push(tile.window().id().clone());
            }
            Some((resize.data, ids))
        });
        for tile in self.tiles_mut(space) {
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
        space: &mut TreeSpace<W>,
        idx: usize,
        new_pos: Point<f64, Logical>,
    ) {
        // Moves up to this logical pixel distance are not animated.
        const ANIMATION_THRESHOLD_SQ: f64 = 10. * 10.;

        let prev_pos = self.container_area(space, idx).loc;
        let new_pos = self.set_container_logical_pos(space, idx, new_pos);

        let diff = prev_pos - new_pos;
        if diff.x * diff.x + diff.y * diff.y > ANIMATION_THRESHOLD_SQ {
            let delta = prev_pos - new_pos;
            let root = self.containers[idx].root;
            for tile in space.tree_mut().tiles_in_branch_mut(root) {
                tile.animate_move_from(delta);
            }
        }
    }

    pub fn new_window_size(
        &self,
        space: &TreeSpace<W>,
        width: Option<PresetSize>,
        height: Option<PresetSize>,
        rules: &ResolvedWindowRules,
    ) -> Size<i32, Logical> {
        let border = space.options().layout.border.merged_with(&rules.border);

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

        let width = resolve(width, space.working_area().size.w);
        let height = resolve(height, space.working_area().size.h);

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
    pub fn wrapper_selected_for_window(&self, space: &TreeSpace<W>, id: &W::Id) -> bool {
        let tree = space.tree();
        self.idx_of(space, id)
            .is_some_and(|idx| tree.selected_container_key() == Some(self.containers[idx].root))
    }

    #[cfg(test)]
    pub fn root_layout_for_window(&self, space: &TreeSpace<W>, id: &W::Id) -> Option<Layout> {
        let tree = space.tree();
        let idx = self.idx_of(space, id)?;
        tree.branch_layout(self.containers[idx].root)
    }

    #[cfg(test)]
    pub fn debug_tree_for_window(&self, space: &TreeSpace<W>, id: &W::Id) -> Option<String>
    where
        W::Id: std::fmt::Display,
    {
        let tree = space.tree();
        let idx = self.idx_of(space, id)?;
        Some(tree.debug_branch(self.containers[idx].root))
    }

    #[cfg(test)]
    pub fn verify_invariants(&self, space: &TreeSpace<W>) {
        use std::collections::HashSet;

        let tree = space.tree();
        assert!(space.scale() > 0.);
        assert!(space.scale().is_finite());

        let stack_roots: HashSet<_> = self
            .containers
            .iter()
            .map(|container| container.root)
            .collect();
        assert_eq!(
            stack_roots.len(),
            self.containers.len(),
            "the floating stack must not contain duplicate roots"
        );
        let tree_roots: HashSet<_> = tree.floating_roots().collect();
        assert_eq!(
            stack_roots, tree_roots,
            "the floating stack must cover every node marked as a floating root"
        );

        for container in &self.containers {
            use crate::layout::SizingMode;

            tree.floating_container_area(container.root)
                .expect("every floating stack entry must name a floating root");
            for tile in tree.tiles_in_branch(container.root) {
                assert!(Rc::ptr_eq(space.options(), &tile.options));
                assert_eq!(space.view_size(), tile.view_size());
                assert_eq!(*space.clock(), tile.clock);
                assert_eq!(space.scale(), tile.scale());
                tile.verify_invariants();

                if let Some(idx) = tile.floating_preset_width_idx {
                    assert!(idx < space.options().layout.preset_column_widths.len());
                }
                if let Some(idx) = tile.floating_preset_height_idx {
                    assert!(idx < space.options().layout.preset_window_heights.len());
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

        if let Some(id) = self.active_window_id(space) {
            assert!(!self.containers.is_empty());
            assert!(
                self.contains(space, &id),
                "active window must be present in tiles"
            );
        } else {
            assert!(self.containers.is_empty());
        }

        if let Some(resize) = &self.interactive_resize {
            assert!(
                self.contains(space, &resize.window),
                "interactive resize window must be present in tiles"
            );
        }
    }
}

impl<W: LayoutElement> FloatingSpace<W> {
    pub(crate) fn layout_tree_nodes(&self, space: &TreeSpace<W>) -> Vec<LayoutTreeNode> {
        let tree = space.tree();
        self.containers
            .iter()
            .enumerate()
            .filter_map(|(idx, container)| {
                let focused_key = tree
                    .selected_node_key()
                    .filter(|key| tree.is_descendant(*key, container.root));
                let mut path = vec![idx];
                let mut root =
                    tree.layout_tree_for_branch(container.root, focused_key, &mut path, true)?;
                root.floating_root_kind = Some(self.root_kind(space, idx).ipc());
                space.apply_workspace_fullscreen(root)
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
