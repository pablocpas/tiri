use std::cell::RefCell;
use std::cmp::max;
use std::collections::HashMap;
use std::rc::Rc;

use log::warn;
use smithay::backend::renderer::element::Kind;
use smithay::backend::renderer::gles::GlesRenderer;
use smithay::utils::{Logical, Point, Rectangle, Scale, Serial, Size};
use tiri_config::utils::MergeWith as _;
use tiri_config::{PresetSize, RelativeTo};
use tiri_ipc::{ColumnDisplay, LayoutTreeNode, PositionChange, SizeChange, WindowLayout};

use super::closing_window::{ClosingWindow, ClosingWindowRenderElement};
use super::container::{
    ContainerMetrics, ContainerTree, DetachedNode, Direction, InsertParentInfo, Layout,
    LeafLayoutInfo, NodeKey, RootPolicy, TabBarInfo, TakenSubtree,
};
use super::focus_ring::{
    render_container_selection, ContainerSelectionStyle, FocusRingEdges, FocusRingRenderElement,
};
use super::legacy_column::ColumnWidth;
use super::tile::{Tile, TileRenderElement, TileRenderSnapshot};
use super::workspace::{InteractiveResize, ResolvedSize};
use super::{
    resize_edges_for_point, ConfigureIntent, InteractiveResizeData, LayoutElement, Options,
    RemovedTile, SizeFrac,
};
use crate::animation::{Animation, Clock};
use crate::layout::tab_bar::{
    render_tab_bar, tab_bar_hit_index, tab_bar_state_from_info, TabBarCacheEntry,
    TabBarRenderOutput,
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
use crate::window::{Mapped, ResolvedWindowRules};

/// By how many logical pixels the directional move commands move floating windows.
pub const DIRECTIONAL_MOVE_PX: f64 = 50.;

/// Space for floating windows.
#[derive(Debug)]
pub struct FloatingSpace<W: LayoutElement> {
    /// Floating containers in top-to-bottom order.
    containers: Vec<FloatingContainer<W>>,

    /// Next floating container id.
    next_container_id: u64,

    /// Id of the active window.
    ///
    /// The active window is not necessarily the topmost window. Focus-follows-mouse should
    /// activate a window, but not bring it to the top, because that's very annoying.
    ///
    /// This is always set to `Some()` when `tiles` isn't empty.
    active_window_id: Option<W::Id>,

    /// Ongoing interactive resize.
    interactive_resize: Option<InteractiveResize<W>>,

    /// Windows in the closing animation.
    closing_windows: Vec<ClosingWindow>,

    /// View size for this space.
    view_size: Size<f64, Logical>,

    /// Working area for this space.
    working_area: Rectangle<f64, Logical>,

    /// Scale of the output the space is on (and rounds its sizes to).
    scale: f64,

    /// Clock for driving animations.
    clock: Clock,

    /// Configurable properties of the layout.
    options: Rc<Options>,

    /// Whether this workspace is active (for tab bar styling).
    is_active: bool,

    /// Currently fullscreened floating window, if any.
    fullscreen_window: Option<W::Id>,

    /// Cached tab bar textures keyed by container id and path.
    tab_bar_cache: RefCell<HashMap<(u64, Vec<usize>), TabBarCacheEntry>>,

    /// Alternate tab bar cache for swap (avoids allocation).
    tab_bar_cache_alt: RefCell<HashMap<(u64, Vec<usize>), TabBarCacheEntry>>,
}

niri_render_elements! {
    FloatingSpaceRenderElement<R> => {
        Tile = TileRenderElement<R>,
        TabBar = PrimaryGpuTextureRenderElement,
        ClosingWindow = ClosingWindowRenderElement,
        ContainerSelection = FocusRingRenderElement,
    }
}

#[derive(Debug)]
struct FloatingContainer<W: LayoutElement> {
    id: u64,
    tree: ContainerTree<W>,
    wrapper_selected: bool,
    workspace_floated: bool,
    data: FloatingContainerData,
    origin: Option<InsertParentInfo>,
}

/// Extra per-container data.
#[derive(Debug, Clone, Copy, PartialEq)]
struct FloatingContainerData {
    /// Position relative to the working area.
    pos: Point<f64, SizeFrac>,

    /// Cached position in logical coordinates.
    ///
    /// Not rounded to physical pixels.
    logical_pos: Point<f64, Logical>,

    /// Cached actual size of the tile.
    size: Size<f64, Logical>,

    /// Working area used for conversions.
    working_area: Rectangle<f64, Logical>,
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

impl FloatingContainerData {
    pub fn new(working_area: Rectangle<f64, Logical>, rect: Rectangle<f64, Logical>) -> Self {
        let mut rv = Self {
            pos: Point::default(),
            logical_pos: Point::default(),
            size: rect.size,
            working_area,
        };
        rv.set_logical_pos(rect.loc);
        rv
    }

    pub fn scale_by_working_area(
        area: Rectangle<f64, Logical>,
        pos: Point<f64, SizeFrac>,
    ) -> Point<f64, Logical> {
        let mut logical_pos = Point::from((pos.x, pos.y));
        logical_pos.x *= area.size.w;
        logical_pos.y *= area.size.h;
        logical_pos += area.loc;
        logical_pos
    }

    pub fn logical_to_size_frac_in_working_area(
        area: Rectangle<f64, Logical>,
        logical_pos: Point<f64, Logical>,
    ) -> Point<f64, SizeFrac> {
        let pos = logical_pos - area.loc;
        let mut pos = Point::from((pos.x, pos.y));
        pos.x /= f64::max(area.size.w, 1.0);
        pos.y /= f64::max(area.size.h, 1.0);
        pos
    }

    fn recompute_logical_pos(&mut self) {
        let mut logical_pos = Self::scale_by_working_area(self.working_area, self.pos);

        // Make sure the window doesn't go too much off-screen. Numbers taken from Mutter.
        let min_on_screen_hor = f64::clamp(self.size.w / 4., 10., 75.);
        let min_on_screen_ver = f64::clamp(self.size.h / 4., 10., 75.);
        let max_off_screen_hor = f64::max(0., self.size.w - min_on_screen_hor);
        let max_off_screen_ver = f64::max(0., self.size.h - min_on_screen_ver);

        logical_pos -= self.working_area.loc;
        logical_pos.x = f64::max(logical_pos.x, -max_off_screen_hor);
        logical_pos.y = f64::max(logical_pos.y, -max_off_screen_ver);
        logical_pos.x = f64::min(
            logical_pos.x,
            self.working_area.size.w - self.size.w + max_off_screen_hor,
        );
        logical_pos.y = f64::min(
            logical_pos.y,
            self.working_area.size.h - self.size.h + max_off_screen_ver,
        );
        logical_pos += self.working_area.loc;

        self.logical_pos = logical_pos;
    }

    pub fn update_config(&mut self, working_area: Rectangle<f64, Logical>) {
        if self.working_area == working_area {
            return;
        }

        self.working_area = working_area;
        self.recompute_logical_pos();
    }

    pub fn set_size(&mut self, size: Size<f64, Logical>) {
        if self.size == size {
            return;
        }

        self.size = size;
        self.recompute_logical_pos();
    }

    pub fn set_logical_pos(&mut self, logical_pos: Point<f64, Logical>) {
        self.pos = Self::logical_to_size_frac_in_working_area(self.working_area, logical_pos);

        // This will clamp the logical position to the current working area.
        self.recompute_logical_pos();
    }

    #[cfg(test)]
    fn verify_invariants(&self) {
        let mut temp = *self;
        temp.recompute_logical_pos();
        assert_eq!(
            self.logical_pos, temp.logical_pos,
            "cached logical pos must be up to date"
        );
    }
}

/// All tiles across the floating containers, in container order.
fn floating_tile_iter<'a, W: LayoutElement>(
    space: &'a FloatingSpace<W>,
) -> impl Iterator<Item = &'a Tile<W>> + 'a {
    space
        .containers
        .iter()
        .flat_map(|container| container.tree.all_tiles())
}

/// All tiles across the floating containers (mutable), in container order.
fn floating_tile_iter_mut<'a, W: LayoutElement>(
    space: &'a mut FloatingSpace<W>,
) -> impl Iterator<Item = &'a mut Tile<W>> + 'a {
    space
        .containers
        .iter_mut()
        .flat_map(|container| container.tree.all_tiles_mut())
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

    fn display_layouts(tree: &ContainerTree<W>) -> &[LeafLayoutInfo] {
        super::tree_space::display_layouts(tree)
    }

    fn container_gap(&self) -> f64 {
        0.0
    }

    fn container_tree_options(&self, options: &Rc<Options>) -> Rc<Options> {
        let gap = self.container_gap();
        if options.layout.gaps == gap {
            return options.clone();
        }

        let mut adjusted = (**options).clone();
        adjusted.layout.gaps = gap;
        Rc::new(adjusted)
    }

    pub fn new(
        view_size: Size<f64, Logical>,
        working_area: Rectangle<f64, Logical>,
        scale: f64,
        clock: Clock,
        options: Rc<Options>,
    ) -> Self {
        Self {
            containers: Vec::new(),
            next_container_id: 1,
            active_window_id: None,
            interactive_resize: None,
            closing_windows: Vec::new(),
            view_size,
            working_area,
            scale,
            clock,
            options,
            tab_bar_cache: RefCell::new(HashMap::new()),
            tab_bar_cache_alt: RefCell::new(HashMap::new()),
            is_active: false,
            fullscreen_window: None,
        }
    }

    pub fn update_config(
        &mut self,
        view_size: Size<f64, Logical>,
        working_area: Rectangle<f64, Logical>,
        scale: f64,
        options: Rc<Options>,
    ) {
        let container_options = self.container_tree_options(&options);
        for container in &mut self.containers {
            container.data.update_config(working_area);
            let local_rect = Rectangle::from_size(container.data.size);
            container.tree.update_config(
                local_rect.size,
                local_rect,
                scale,
                container_options.clone(),
            );
            container.tree.layout();
        }

        for tile in self.tiles_mut() {
            tile.update_config(view_size, scale, options.clone());
        }

        self.view_size = view_size;
        self.working_area = working_area;
        self.scale = scale;
        self.options = options;
    }

    pub fn update_shaders(&mut self) {
        for tile in self.tiles_mut() {
            tile.update_shaders();
        }
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
        self.tiles().any(Tile::are_animations_ongoing) || !self.closing_windows.is_empty()
    }

    pub fn are_transitions_ongoing(&self) -> bool {
        self.tiles().any(Tile::are_transitions_ongoing) || !self.closing_windows.is_empty()
    }

    pub fn update_render_elements(&mut self, is_active: bool, view_rect: Rectangle<f64, Logical>) {
        self.is_active = is_active;
        let active = self.active_window_id.clone();
        let selection_is_container = self
            .active_container_idx()
            .is_some_and(|idx| self.selected_is_container_in(idx));
        let scale = self.scale;
        for container in &mut self.containers {
            let applied = container.tree.apply_pending_layouts_if_ready();
            if applied && container.tree.take_pending_relayout() {
                container.tree.layout();
            }

            let layouts = Self::display_layouts(&container.tree).to_vec();
            for info in layouts {
                if let Some(tile) = container.tree.get_tile_mut(info.key) {
                    let is_fullscreen_tile = self
                        .fullscreen_window
                        .as_ref()
                        .is_some_and(|id| id == tile.window().id());

                    let tile_view_rect = if is_fullscreen_tile {
                        view_rect
                    } else {
                        let mut pos =
                            container.data.logical_pos + info.rect.loc + tile.render_offset();
                        pos = pos.to_physical_precise_round(scale).to_logical(scale);
                        let mut r = view_rect;
                        r.loc -= pos;
                        r
                    };

                    let is_focused = if is_fullscreen_tile {
                        is_active
                    } else {
                        is_active
                            && Some(tile.window().id()) == active.as_ref()
                            && !selection_is_container
                    };
                    tile.update_render_elements(
                        is_active,
                        is_focused,
                        FocusRingEdges::all(),
                        None,
                        tile_view_rect,
                    );
                }
            }
        }
    }

    pub fn tiles(&self) -> impl Iterator<Item = &Tile<W>> + '_ {
        floating_tile_iter(self)
    }

    pub fn tiles_mut(&mut self) -> impl Iterator<Item = &mut Tile<W>> + '_ {
        floating_tile_iter_mut(self)
    }

    pub fn tiles_with_offsets(&self) -> impl Iterator<Item = (&Tile<W>, Point<f64, Logical>)> + '_ {
        let mut tiles = Vec::new();
        for container in &self.containers {
            let offset = container.data.logical_pos;
            for info in Self::display_layouts(&container.tree) {
                if let Some(tile) = container.tree.get_tile(info.key) {
                    tiles.push((tile, offset + info.rect.loc));
                }
            }
        }
        tiles.into_iter()
    }

    pub(super) fn resize_hit_under(&self, pos: Point<f64, Logical>) -> FloatingResizeResult<W::Id> {
        if self.fullscreen_window.is_some() {
            return FloatingResizeResult::None;
        }

        let scale = Scale::from(self.scale);
        let gap = self.container_gap();
        for container in &self.containers {
            let offset = container.data.logical_pos;
            let pos_in_container = pos - offset;
            let tab_bar_infos = container.tree.tab_bar_layouts();
            for info in Self::display_layouts(&container.tree)
                .iter()
                .filter(|info| info.visible)
            {
                let Some(tile) = container.tree.get_tile(info.key) else {
                    continue;
                };

                let mut tile_pos = offset + info.rect.loc + tile.render_offset();
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
                    pos_in_container,
                    gap,
                    self.scale,
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

                let external_edges =
                    Self::external_edges_for_rect(container.data.size, info.rect, edges);
                return FloatingResizeResult::Hit(FloatingResizeHit {
                    window: tile.window().id().clone(),
                    edges,
                    external_edges,
                });
            }
        }

        FloatingResizeResult::None
    }

    pub fn resize_edges_under(&self, pos: Point<f64, Logical>) -> Option<ResizeEdge> {
        match self.resize_hit_under(pos) {
            FloatingResizeResult::Hit(hit) => Some(hit.edges),
            FloatingResizeResult::Blocked => Some(ResizeEdge::empty()),
            FloatingResizeResult::None => None,
        }
    }

    fn tiles_with_offsets_visible(
        &self,
    ) -> impl Iterator<Item = (&Tile<W>, Point<f64, Logical>)> + '_ {
        let mut tiles = Vec::new();
        for container in &self.containers {
            let offset = container.data.logical_pos;
            for info in Self::display_layouts(&container.tree)
                .iter()
                .filter(|info| info.visible)
            {
                if let Some(tile) = container.tree.get_tile(info.key) {
                    tiles.push((tile, offset + info.rect.loc));
                }
            }
        }
        tiles.into_iter()
    }

    pub fn tiles_with_offsets_mut(
        &mut self,
    ) -> impl Iterator<Item = (&mut Tile<W>, Point<f64, Logical>)> + '_ {
        self.containers.iter_mut().flat_map(|container| {
            let offset = container.data.logical_pos;
            let layouts = Self::display_layouts(&container.tree).to_vec();
            let keys: Vec<NodeKey> = layouts.iter().map(|info| info.key).collect();
            let locs: Vec<Point<f64, Logical>> = layouts.iter().map(|info| info.rect.loc).collect();
            container
                .tree
                .tiles_mut_for_keys(&keys)
                .into_iter()
                .map(move |(idx, tile)| (tile, offset + locs[idx]))
        })
    }

    pub fn tiles_with_render_positions(
        &self,
    ) -> impl Iterator<Item = (&Tile<W>, Point<f64, Logical>)> {
        let scale = self.scale;
        self.tiles_with_offsets_visible()
            .map(move |(tile, offset)| {
                let pos = offset + tile.render_offset();
                // Round to physical pixels.
                let pos = pos.to_physical_precise_round(scale).to_logical(scale);
                (tile, pos)
            })
    }

    fn tab_bar_hit(&self, pos: Point<f64, Logical>) -> Option<(&W, super::HitType)> {
        if self.options.layout.tab_bar.off {
            return None;
        }

        let cache = self.tab_bar_cache.borrow();
        let gap = self.container_gap();

        for container in &self.containers {
            for mut info in container.tree.tab_bar_layouts() {
                if gap > 0.0 && info.path.is_empty() {
                    info.rect.loc.x -= gap;
                    info.rect.loc.y -= gap;
                    info.rect.size.w = (info.rect.size.w + gap * 2.0).max(0.0);
                }
                info.rect.loc += container.data.logical_pos;

                let key = (container.id, info.path.clone());
                let cached_widths = cache.get(&key).map(|entry| entry.tab_widths_px.as_slice());
                // A 1px pad makes the floating bar's edges forgiving to hit.
                let Some(tab_idx) = tab_bar_hit_index(&info, pos, self.scale, cached_widths, 1)
                else {
                    continue;
                };

                if let Some(window) = container.tree.window_for_tab(&info.path, tab_idx) {
                    return Some((
                        window,
                        super::HitType::Activate {
                            is_tab_indicator: true,
                        },
                    ));
                }
            }
        }

        None
    }

    pub fn window_under(&self, pos: Point<f64, Logical>) -> Option<(&W, super::HitType)> {
        if let Some(fullscreen_id) = &self.fullscreen_window {
            let tile = self.tiles().find(|t| t.window().id() == fullscreen_id)?;
            return super::HitType::hit_tile(tile, Point::from((0.0, 0.0)), pos);
        }

        if let Some(hit) = self.tab_bar_hit(pos) {
            return Some(hit);
        }

        for (tile, tile_pos) in self.tiles_with_render_positions() {
            if let Some(rv) = super::HitType::hit_tile(tile, tile_pos, pos) {
                return Some(rv);
            }
        }

        None
    }

    pub fn tiles_with_render_positions_mut(
        &mut self,
        round: bool,
    ) -> impl Iterator<Item = (&mut Tile<W>, Point<f64, Logical>)> {
        let scale = self.scale;
        self.tiles_with_offsets_mut().map(move |(tile, offset)| {
            let mut pos = offset + tile.render_offset();
            // Round to physical pixels.
            if round {
                pos = pos.to_physical_precise_round(scale).to_logical(scale);
            }
            (tile, pos)
        })
    }

    pub fn tiles_with_ipc_layouts(&self) -> impl Iterator<Item = (&Tile<W>, WindowLayout)> {
        let scale = self.scale;
        self.tiles_with_offsets().map(move |(tile, offset)| {
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

    pub fn new_window_toplevel_bounds(&self, rules: &ResolvedWindowRules) -> Size<i32, Logical> {
        let border_config = self.options.layout.border.merged_with(&rules.border);
        compute_toplevel_bounds(border_config, self.working_area.size)
    }

    /// Returns the geometry of the active window relative to and clamped to the working area.
    ///
    /// During animations, assumes the final tile position.
    pub fn active_window_visual_rectangle(&self) -> Option<Rectangle<f64, Logical>> {
        let active_id = self.active_window_id.as_ref()?;
        let (tile, offset) = self
            .tiles_with_offsets_visible()
            .find(|(tile, _)| tile.window().id() == active_id)?;

        let window_pos = offset + tile.window_loc();
        let window_size = tile.window_size();
        let window_rect = Rectangle::new(window_pos, window_size);

        self.working_area.intersection(window_rect)
    }

    pub fn popup_target_rect(&self, id: &W::Id) -> Option<Rectangle<f64, Logical>> {
        for (tile, pos) in self.tiles_with_offsets_visible() {
            if tile.window().id() == id {
                // Position within the working area.
                let mut target = self.working_area;
                target.loc -= pos;
                target.loc -= tile.window_loc();

                return Some(target);
            }
        }
        None
    }

    fn idx_of(&self, id: &W::Id) -> Option<usize> {
        self.containers
            .iter()
            .position(|container| container.tree.window_key(id).is_some())
    }

    fn contains(&self, id: &W::Id) -> bool {
        self.idx_of(id).is_some()
    }

    fn active_container_idx(&self) -> Option<usize> {
        let active_id = self.active_window_id.as_ref()?;
        self.idx_of(active_id)
    }

    fn selected_is_container_in(&self, idx: usize) -> bool {
        self.containers[idx].wrapper_selected || self.containers[idx].tree.selected_is_container()
    }

    /// The node a command targets inside container `idx`: its root when the whole floating
    /// wrapper is selected, otherwise the tree's own selection.
    fn selected_key_in(&self, idx: usize) -> Option<NodeKey> {
        let tree = &self.containers[idx].tree;
        if self.containers[idx].wrapper_selected {
            tree.root_node_key()
        } else {
            tree.selected_node_key()
        }
    }

    fn tile_at_mut(&mut self, id: &W::Id) -> Option<&mut Tile<W>> {
        for container in &mut self.containers {
            if let Some(key) = container.tree.window_key(id) {
                return container.tree.get_tile_mut(key);
            }
        }
        None
    }

    pub fn active_window(&self) -> Option<&W> {
        let id = self.active_window_id.as_ref()?;
        self.tiles()
            .find(|tile| tile.window().id() == id)
            .map(Tile::window)
    }

    pub fn active_window_mut(&mut self) -> Option<&mut W> {
        let id = self.active_window_id.clone()?;
        self.tiles_mut()
            .find(|tile| tile.window().id() == &id)
            .map(Tile::window_mut)
    }

    pub fn has_window(&self, id: &W::Id) -> bool {
        self.containers
            .iter()
            .any(|container| container.tree.window_key(id).is_some())
    }

    pub fn is_empty(&self) -> bool {
        self.containers.is_empty()
    }

    pub fn set_fullscreen(&mut self, window: &W::Id, is_fullscreen: bool) {
        if is_fullscreen {
            if self
                .fullscreen_window
                .as_ref()
                .is_some_and(|id| id == window)
            {
                return;
            }

            if let Some(previous) = self.fullscreen_window.clone() {
                if previous != *window {
                    self.set_fullscreen(&previous, false);
                }
            }

            // Store the floating size before going fullscreen.
            if let Some(tile) = self.tiles_mut().find(|t| t.window().id() == window) {
                Self::store_floating_size_for_restore(tile);
                tile.request_fullscreen(true, None);
            }

            self.fullscreen_window = Some(window.clone());
        } else {
            if !self
                .fullscreen_window
                .as_ref()
                .is_some_and(|id| id == window)
            {
                return;
            }

            // Restore the floating size.
            if let Some(tile) = self.tiles_mut().find(|t| t.window().id() == window) {
                let size = tile.floating_window_size.unwrap_or_default();
                tile.window_mut().request_size_once(size, true);
            }

            self.fullscreen_window = None;
        }
    }

    pub fn is_fullscreen(&self, window: &W::Id) -> bool {
        self.fullscreen_window
            .as_ref()
            .is_some_and(|id| id == window)
    }

    pub fn has_fullscreen_window(&self) -> bool {
        self.fullscreen_window.is_some()
    }

    pub fn selected_is_container(&self, id: Option<&W::Id>) -> bool {
        let Some(id) = id.or(self.active_window_id.as_ref()) else {
            return false;
        };
        let Some(idx) = self.idx_of(id) else {
            return false;
        };
        self.selected_is_container_in(idx)
    }

    pub(super) fn active_wrapper_selected(&self) -> bool {
        let Some(idx) = self.active_container_idx() else {
            return false;
        };
        self.containers[idx].wrapper_selected
    }

    pub(super) fn active_command_container_selected(&self) -> bool {
        let Some(idx) = self.active_container_idx() else {
            return false;
        };
        self.containers[idx].wrapper_selected || self.containers[idx].tree.selected_is_container()
    }

    pub(super) fn active_command_container_path(&self) -> Option<Vec<usize>> {
        let idx = self.active_container_idx()?;
        if self.containers[idx].wrapper_selected {
            return Some(Vec::new());
        }
        if self.containers[idx].tree.selected_is_container() {
            return Some(self.containers[idx].tree.selected_path());
        }
        None
    }

    pub(super) fn close_window_ids_for_active_selection(&self) -> Vec<W::Id> {
        let Some(idx) = self.active_container_idx() else {
            return Vec::new();
        };
        if self.containers[idx].wrapper_selected {
            return self.containers[idx].tree.all_window_ids();
        }

        if self.containers[idx].tree.selected_is_container() {
            if let Some(key) = self.containers[idx].tree.selected_node_key() {
                return self.containers[idx].tree.window_ids_under(key);
            }
        }

        self.active_window_id
            .as_ref()
            .map(|id| vec![id.clone()])
            .unwrap_or_default()
    }

    pub(super) fn select_wrapper_for_window(&mut self, id: &W::Id) -> bool {
        let Some(idx) = self.idx_of(id) else {
            return false;
        };
        self.containers[idx].tree.clear_selection();
        self.containers[idx].wrapper_selected = true;
        true
    }

    pub fn clear_selection_context(&mut self) {
        for container in &mut self.containers {
            container.wrapper_selected = false;
            container.tree.clear_selection();
        }
    }

    pub fn add_tile(&mut self, tile: Tile<W>, activate: bool) {
        self.add_tile_at(0, tile, activate);
    }

    pub fn add_tile_with_restore_hint(&mut self, mut tile: Tile<W>, activate: bool) {
        let hint = tile.floating_reinsert_hint.take();

        if let Some((container_id, insert_info)) = hint {
            if let Some(idx) = self
                .containers
                .iter()
                .position(|container| container.id == container_id)
            {
                self.add_tile_to_container_idx_with_parent_info(idx, tile, activate, &insert_info);
                return;
            }
        }

        self.add_tile(tile, activate);
    }

    fn prepare_tile_for_floating(
        &mut self,
        tile: &mut Tile<W>,
    ) -> (W::Id, Option<Size<f64, Logical>>) {
        tile.update_config(self.view_size, self.scale, self.options.clone());
        tile.pending_maximized = false;

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

    fn add_tile_at(&mut self, mut idx: usize, mut tile: Tile<W>, activate: bool) {
        let (win_id, requested_tile_size) = self.prepare_tile_for_floating(&mut tile);

        if activate || self.containers.is_empty() {
            self.active_window_id = Some(win_id.clone());
        }

        // Make sure the tile isn't inserted below its parent.
        for (i, container) in self.containers.iter().enumerate().take(idx) {
            if container
                .tree
                .all_windows()
                .iter()
                .any(|parent| tile.window().is_child_of(parent))
            {
                idx = i;
                break;
            }
        }

        let tile_size = requested_tile_size.unwrap_or_else(|| tile.tile_size());
        let pos = self
            .stored_or_default_tile_pos(&tile)
            .unwrap_or_else(|| center_preferring_top_left_in_area(self.working_area, tile_size));
        let rect = Rectangle::new(pos, tile_size);

        let mut tree = ContainerTree::new(
            rect.size,
            Rectangle::from_size(rect.size),
            self.scale,
            self.container_tree_options(&self.options),
        );
        tree.insert_leaf_at(0, tile, activate);
        if activate {
            tree.focus_window_by_id(&win_id);
        }
        tree.layout();

        let container = FloatingContainer {
            id: self.next_container_id,
            tree,
            wrapper_selected: false,
            workspace_floated: false,
            data: FloatingContainerData::new(self.working_area, rect),
            origin: None,
        };
        self.next_container_id += 1;

        self.containers.insert(idx, container);
        self.bring_up_descendants_of(idx);
    }

    pub(super) fn add_tile_to_active_container(&mut self, tile: Tile<W>, activate: bool) -> bool {
        let Some(idx) = self.active_container_idx() else {
            return false;
        };
        self.add_tile_to_container_idx(idx, tile, activate)
    }

    pub(super) fn add_tile_to_container_of(
        &mut self,
        id: &W::Id,
        tile: Tile<W>,
        activate: bool,
    ) -> bool {
        let Some(idx) = self.idx_of(id) else {
            return false;
        };

        self.add_tile_to_container_idx(idx, tile, activate)
    }

    fn add_tile_to_container_idx(&mut self, idx: usize, mut tile: Tile<W>, activate: bool) -> bool {
        let (win_id, _) = self.prepare_tile_for_floating(&mut tile);
        if self.containers[idx].wrapper_selected {
            let insert_idx = self.containers[idx].tree.root_children_len();
            self.containers[idx]
                .tree
                .insert_leaf_at(insert_idx, tile, activate);
        } else {
            self.containers[idx]
                .tree
                .insert_window_with_focus(tile, activate);
        }
        self.containers[idx].tree.layout();

        if activate {
            self.activate_window(&win_id);
            self.containers[idx].wrapper_selected = false;
        } else if self.active_window_id.is_none() {
            self.active_window_id = Some(win_id);
        }

        true
    }

    fn add_tile_to_container_idx_with_parent_info(
        &mut self,
        idx: usize,
        mut tile: Tile<W>,
        activate: bool,
        info: &InsertParentInfo,
    ) {
        let (win_id, _) = self.prepare_tile_for_floating(&mut tile);

        let _ = self.containers[idx]
            .tree
            .insert_leaf_with_parent_info(info, tile, activate);
        self.containers[idx].tree.layout();

        if activate {
            self.activate_window(&win_id);
        } else if self.active_window_id.is_none() {
            self.active_window_id = Some(win_id);
        }
    }

    pub(super) fn active_container_allows_splits(&self) -> bool {
        let Some(idx) = self.active_container_idx() else {
            return false;
        };
        self.containers[idx].tree.focused_container_allows_splits()
    }

    pub(super) fn container_allows_splits(&self, id: &W::Id) -> bool {
        let Some(idx) = self.idx_of(id) else {
            return false;
        };
        self.containers[idx].tree.focused_container_allows_splits()
    }

    pub(super) fn container_pos(&self, id: &W::Id) -> Option<Point<f64, Logical>> {
        let idx = self.idx_of(id)?;
        Some(self.containers[idx].data.logical_pos)
    }

    pub(super) fn move_container_for_window_to(
        &mut self,
        id: &W::Id,
        pos: Point<f64, Logical>,
        animate: bool,
    ) -> bool {
        let Some(idx) = self.idx_of(id) else {
            return false;
        };
        self.move_container_to(idx, pos, animate);
        true
    }

    pub fn add_tile_above(&mut self, above: &W::Id, mut tile: Tile<W>, activate: bool) {
        let idx = self.idx_of(above).unwrap();

        let above_pos = self.containers[idx].data.logical_pos;
        let above_size = self.containers[idx].data.size;
        let tile_size = tile.tile_size();
        let pos = above_pos + (above_size.to_point() - tile_size.to_point()).downscale(2.);
        let pos = self.clamp_within_working_area(pos, tile_size);
        tile.floating_pos = Some(self.logical_to_size_frac(pos));

        self.add_tile_at(idx, tile, activate);
    }

    pub(super) fn add_subtree(
        &mut self,
        mut subtree: DetachedNode<W>,
        rect: Rectangle<f64, Logical>,
        origin: Option<InsertParentInfo>,
        activate: bool,
        focus: Option<&W::Id>,
        workspace_floated: bool,
    ) {
        subtree.for_each_tile_mut(&mut |tile| {
            self.prepare_tile_for_floating(tile);
        });

        let mut tree = ContainerTree::new(
            rect.size,
            Rectangle::from_size(rect.size),
            self.scale,
            self.container_tree_options(&self.options),
        );
        tree.insert_subtree_with_focus(subtree, activate);
        if let Some(id) = focus {
            tree.focus_window_by_id(id);
        }
        tree.layout();

        let focus_id = focus
            .cloned()
            .or_else(|| tree.focused_window().map(|win| win.id().clone()));

        let container = FloatingContainer {
            id: self.next_container_id,
            tree,
            wrapper_selected: false,
            workspace_floated,
            data: FloatingContainerData::new(self.working_area, rect),
            origin,
        };
        self.next_container_id += 1;

        if activate || self.containers.is_empty() {
            self.active_window_id = focus_id;
        }

        self.containers.insert(0, container);
        self.bring_up_descendants_of(0);
    }

    fn bring_up_descendants_of(&mut self, idx: usize) {
        let base_windows = self.containers[idx].tree.all_windows();
        let mut seen_windows = base_windows;
        let mut descendants: Vec<usize> = Vec::new();

        for (i, container_below) in self.containers.iter().enumerate().skip(idx + 1).rev() {
            let windows = container_below.tree.all_windows();
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

    pub fn remove_active_tile(&mut self) -> Option<RemovedTile<W>> {
        let id = self.active_window_id.clone()?;
        Some(self.remove_tile(&id))
    }

    pub fn remove_tile(&mut self, id: &W::Id) -> RemovedTile<W> {
        let idx = self.idx_of(id).unwrap();
        self.remove_tile_from_container(idx, id)
    }

    pub(super) fn take_container_subtree(&mut self, id: &W::Id) -> Option<TakenSubtree<W>> {
        // Clear fullscreen if the subtree contains the fullscreen window.
        if let Some(fs_id) = &self.fullscreen_window {
            if self.idx_of(fs_id) == self.idx_of(id) {
                self.fullscreen_window = None;
            }
        }

        let idx = self.idx_of(id)?;

        if let Some(resize) = &self.interactive_resize {
            if self.idx_of(&resize.window) == Some(idx) {
                self.interactive_resize = None;
            }
        }

        let mut container = self.containers.remove(idx);
        let rect = Rectangle::new(container.data.logical_pos, container.data.size);
        let origin = container.origin.take();
        let root_key = container.tree.root_node_key()?;
        let (mut subtree, _insert_info) = container.tree.take_subtree_at(root_key)?;
        subtree.for_each_tile_mut(&mut |tile| {
            Self::store_floating_size_for_restore(tile);
        });
        // The reference container_set_floating(false) does NOT collapse/normalize the tree.
        // Insert directly using the inactive tiling reference.

        if let Some(active) = &self.active_window_id {
            if !self.contains(active) {
                self.active_window_id = self.containers.first().and_then(|container| {
                    container.tree.focused_window().map(|win| win.id().clone())
                });
            }
        }

        Some((subtree, origin, rect))
    }

    pub(super) fn active_container_is_workspace_floated(&self) -> bool {
        self.active_window_id
            .as_ref()
            .and_then(|id| self.idx_of(id))
            .is_some_and(|idx| self.containers[idx].workspace_floated)
    }

    fn remove_tile_from_container(&mut self, idx: usize, id: &W::Id) -> RemovedTile<W> {
        let container_pos = self.containers[idx].data.pos;
        let container_id = self.containers[idx].id;
        let insert_hint = self.containers[idx].tree.insert_parent_info_for_window(id);
        let mut tile = {
            let container = &mut self.containers[idx];
            container
                .tree
                .remove_window(id)
                .expect("window must exist in floating container")
        };

        if Some(tile.window().id()) == self.active_window_id.as_ref() {
            self.active_window_id = None;
        }

        // Clear fullscreen if this was the fullscreen window.
        if self.fullscreen_window.as_ref() == Some(tile.window().id()) {
            self.fullscreen_window = None;
        }

        // Stop interactive resize.
        if let Some(resize) = &self.interactive_resize {
            if tile.window().id() == &resize.window {
                self.interactive_resize = None;
            }
        }

        if self.containers[idx].tree.window_count() == 0 {
            self.containers.remove(idx);
        }

        if self.active_window_id.is_none() {
            self.active_window_id = self
                .containers
                .first()
                .and_then(|container| container.tree.focused_window().map(|win| win.id().clone()));
        }

        Self::store_floating_size_for_restore(&mut tile);
        // Store the floating position.
        tile.floating_pos = Some(container_pos);
        tile.floating_reinsert_hint = insert_hint.map(|info| (container_id, info));

        let width = ColumnWidth::Fixed(tile.tile_expected_or_current_size().w as i32);
        RemovedTile {
            tile,
            width,
            is_full_width: false,
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
        renderer: &mut GlesRenderer,
        id: &W::Id,
        blocker: TransactionBlocker,
    ) {
        let (tile, tile_pos) = self
            .tiles_with_render_positions_mut(false)
            .find(|(tile, _)| tile.window().id() == id)
            .unwrap();

        let Some(snapshot) = tile.take_unmap_snapshot() else {
            return;
        };

        let tile_size = tile.tile_size();

        self.start_close_animation_for_tile(renderer, snapshot, tile_size, tile_pos, blocker);
    }

    pub fn activate_window_without_raising(&mut self, id: &W::Id) -> bool {
        let Some(idx) = self.idx_of(id) else {
            return false;
        };

        self.containers[idx].wrapper_selected = false;
        self.containers[idx].tree.clear_selection();
        let _ = self.containers[idx].tree.focus_window_by_id(id);
        self.active_window_id = Some(id.clone());
        true
    }

    pub fn activate_window(&mut self, id: &W::Id) -> bool {
        let Some(idx) = self.idx_of(id) else {
            return false;
        };

        self.raise_container(idx, 0);
        self.active_window_id = Some(id.clone());
        self.bring_up_descendants_of(0);
        if let Some(idx) = self.idx_of(id) {
            self.containers[idx].wrapper_selected = false;
            self.containers[idx].tree.clear_selection();
            let _ = self.containers[idx].tree.focus_window_by_id(id);
        }

        true
    }

    fn raise_container(&mut self, from_idx: usize, to_idx: usize) {
        assert!(to_idx <= from_idx);

        let container = self.containers.remove(from_idx);
        self.containers.insert(to_idx, container);
    }

    pub fn start_close_animation_for_tile(
        &mut self,
        renderer: &mut GlesRenderer,
        snapshot: TileRenderSnapshot,
        tile_size: Size<f64, Logical>,
        tile_pos: Point<f64, Logical>,
        blocker: TransactionBlocker,
    ) {
        let anim = Animation::new(
            self.clock.clone(),
            0.,
            1.,
            0.,
            self.options.animations.window_close.anim,
        );

        let scale = Scale::from(self.scale);
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

    fn resolve_target_id(&self, id: Option<&W::Id>) -> Option<W::Id> {
        id.cloned().or_else(|| self.active_window_id.clone())
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

    pub fn toggle_window_width(&mut self, id: Option<&W::Id>, forwards: bool) {
        let Some(id) = self.resolve_target_id(id) else {
            return;
        };
        let available_size = self.working_area.size.w;
        let presets = self.options.layout.preset_column_widths.clone();

        let Some(tile) = self.tile_at_mut(&id) else {
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
        self.set_window_width(Some(&id), SizeChange::from(preset), true);

        if let Some(tile) = self.tile_at_mut(&id) {
            tile.floating_preset_width_idx = Some(preset_idx);
        }

        self.interactive_resize_end(Some(&id));
    }

    pub fn start_open_animation(&mut self, id: &W::Id) -> bool {
        if let Some(tile) = self.tile_at_mut(id) {
            tile.start_open_animation();
            true
        } else {
            false
        }
    }

    pub fn toggle_window_height(&mut self, id: Option<&W::Id>, forwards: bool) {
        let Some(id) = self.resolve_target_id(id) else {
            return;
        };
        let available_size = self.working_area.size.h;
        let presets = self.options.layout.preset_window_heights.clone();

        let Some(tile) = self.tile_at_mut(&id) else {
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
        self.set_window_height(Some(&id), SizeChange::from(preset), true);

        if let Some(tile) = self.tile_at_mut(&id) {
            tile.floating_preset_height_idx = Some(preset_idx);
        }

        self.interactive_resize_end(Some(&id));
    }

    fn container_metrics(
        &self,
        tree: &ContainerTree<W>,
        key: NodeKey,
        layout: Layout,
    ) -> Option<ContainerMetrics> {
        let (parent_key, child_idx) = tree.find_parent_with_layout(key, layout)?;
        let (container_layout, rect, child_count) = tree.container_info(parent_key)?;
        if container_layout != layout || child_count == 0 {
            return None;
        }

        let available = match layout {
            Layout::SplitH => self.available_span(rect.size.w, child_count),
            Layout::SplitV => self.available_span(rect.size.h, child_count),
            Layout::Tabbed | Layout::Stacked => return None,
        };

        if available <= 0.0 {
            return None;
        }

        Some((parent_key, child_idx, available, child_count, rect))
    }

    fn available_span(&self, total: f64, child_count: usize) -> f64 {
        super::tree_space::available_span(self.container_gap(), total, child_count)
    }

    fn percent_from_size_change(current_percent: f64, available: f64, change: SizeChange) -> f64 {
        if available <= 0.0 {
            return current_percent;
        }

        let to_proportion = |value: f64| {
            if value.abs() > 1.0 {
                value / available
            } else {
                value
            }
        };

        let adjust = |percent: f64, delta: f64| (percent + delta).clamp(0.05, 1.0);

        match change {
            SizeChange::SetProportion(percent) => to_proportion(percent / 100.),
            SizeChange::AdjustProportion(percent) => adjust(current_percent, percent / 100.),
            SizeChange::SetFixed(size) => {
                let percent = f64::from(size) / available;
                percent.clamp(0.05, 1.0)
            }
            SizeChange::AdjustFixed(delta) => {
                let percent = f64::from(delta) / available;
                adjust(current_percent, percent)
            }
        }
    }

    fn resize_container_dimension(
        &mut self,
        idx: usize,
        change: SizeChange,
        is_width: bool,
        animate: bool,
    ) {
        let available = if is_width {
            self.working_area.size.w
        } else {
            self.working_area.size.h
        };
        let current = if is_width {
            self.containers[idx].data.size.w
        } else {
            self.containers[idx].data.size.h
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
            Size::from((new_size, self.containers[idx].data.size.h))
        } else {
            Size::from((self.containers[idx].data.size.w, new_size))
        };
        self.containers[idx].data.set_size(size);

        let rect = Rectangle::from_size(self.containers[idx].data.size);
        self.containers[idx].tree.set_view_size(rect.size, rect);
        if animate {
            self.containers[idx].tree.layout();
        } else {
            self.containers[idx]
                .tree
                .layout_with_animation_flags(false, false);
        }
    }

    pub fn set_window_width(&mut self, id: Option<&W::Id>, change: SizeChange, animate: bool) {
        let Some(target_id) = id.or(self.active_window_id.as_ref()) else {
            return;
        };
        let idx = self.idx_of(target_id).unwrap();
        let selection_is_container = id.is_none() && self.selected_is_container_in(idx);
        if selection_is_container {
            self.resize_container_dimension(idx, change, true, animate);
            return;
        }

        let key = if let Some(id) = id {
            match self.containers[idx].tree.window_key(id) {
                Some(key) => key,
                None => return,
            }
        } else {
            match self.selected_key_in(idx) {
                Some(key) => key,
                None => return,
            }
        };

        if let Some(tile) = self.containers[idx].tree.get_tile_mut(key) {
            tile.floating_preset_width_idx = None;
        }

        let Some((parent_path, child_idx, available, child_count, _)) =
            self.container_metrics(&self.containers[idx].tree, key, Layout::SplitH)
        else {
            self.resize_container_dimension(idx, change, true, animate);
            return;
        };
        if child_count <= 1 {
            self.resize_container_dimension(idx, change, true, animate);
            return;
        }

        let current_percent = self.containers[idx]
            .tree
            .child_percent(parent_path, child_idx)
            .unwrap_or(1.0);
        let percent = Self::percent_from_size_change(current_percent, available, change);

        if self.containers[idx].tree.set_child_percent(
            parent_path,
            child_idx,
            Layout::SplitH,
            percent,
        ) {
            if animate {
                self.containers[idx].tree.layout();
            } else {
                self.containers[idx]
                    .tree
                    .layout_with_animation_flags(false, false);
            }
        }
    }

    pub fn set_window_height(&mut self, id: Option<&W::Id>, change: SizeChange, animate: bool) {
        let Some(target_id) = id.or(self.active_window_id.as_ref()) else {
            return;
        };
        let idx = self.idx_of(target_id).unwrap();
        let selection_is_container = id.is_none() && self.selected_is_container_in(idx);
        if selection_is_container {
            self.resize_container_dimension(idx, change, false, animate);
            return;
        }

        let key = if let Some(id) = id {
            match self.containers[idx].tree.window_key(id) {
                Some(key) => key,
                None => return,
            }
        } else {
            match self.selected_key_in(idx) {
                Some(key) => key,
                None => return,
            }
        };

        if let Some(tile) = self.containers[idx].tree.get_tile_mut(key) {
            tile.floating_preset_height_idx = None;
        }

        let Some((parent_path, child_idx, available, child_count, _)) =
            self.container_metrics(&self.containers[idx].tree, key, Layout::SplitV)
        else {
            self.resize_container_dimension(idx, change, false, animate);
            return;
        };
        if child_count <= 1 {
            self.resize_container_dimension(idx, change, false, animate);
            return;
        }

        let current_percent = self.containers[idx]
            .tree
            .child_percent(parent_path, child_idx)
            .unwrap_or(1.0);
        let percent = Self::percent_from_size_change(current_percent, available, change);

        if self.containers[idx].tree.set_child_percent(
            parent_path,
            child_idx,
            Layout::SplitV,
            percent,
        ) {
            if animate {
                self.containers[idx].tree.layout();
            } else {
                self.containers[idx]
                    .tree
                    .layout_with_animation_flags(false, false);
            }
        }
    }

    fn focus_directional(
        &mut self,
        distance: impl Fn(Point<f64, Logical>, Point<f64, Logical>) -> f64,
    ) -> bool {
        let Some(active_id) = &self.active_window_id else {
            return false;
        };
        let (active_tile, active_pos) = match self
            .tiles_with_offsets_visible()
            .find(|(tile, _)| tile.window().id() == active_id)
        {
            Some(value) => value,
            None => return false,
        };
        let center = active_pos + active_tile.tile_size().downscale(2.);

        let result = self
            .tiles_with_offsets_visible()
            .filter(|(tile, _)| tile.window().id() != active_id)
            .map(|(tile, pos)| {
                let other_center = pos + tile.tile_size().downscale(2.);
                (tile, distance(center, other_center))
            })
            .filter(|(_, dist)| *dist > 0.)
            .min_by(|(_, dist_a), (_, dist_b)| f64::total_cmp(dist_a, dist_b));
        if let Some((tile, _)) = result {
            let id = tile.window().id().clone();
            self.activate_window(&id);
            true
        } else {
            false
        }
    }

    fn focus_within_active_container(&mut self, direction: Direction, allow_wrap: bool) -> bool {
        let Some(idx) = self.active_container_idx() else {
            return false;
        };
        let moved = if self.selected_is_container_in(idx) || !allow_wrap {
            self.containers[idx]
                .tree
                .focus_in_direction_no_wrap(direction)
        } else {
            self.containers[idx].tree.focus_in_direction(direction)
        };
        if moved {
            if let Some(win) = self.containers[idx].tree.focused_window() {
                self.active_window_id = Some(win.id().clone());
            }
            return true;
        }

        false
    }

    fn focus_in_stack_order(&mut self, delta: isize) -> bool {
        if self.containers.len() <= 1 {
            return false;
        }

        let Some(idx) = self.active_container_idx() else {
            return false;
        };
        let len = self.containers.len() as isize;
        let target_idx = (idx as isize + delta).rem_euclid(len) as usize;
        if target_idx == idx {
            return false;
        }

        let Some(id) = self.containers[target_idx]
            .tree
            .focused_window()
            .map(|win| win.id().clone())
            .or_else(|| {
                self.containers[target_idx]
                    .tree
                    .all_windows()
                    .into_iter()
                    .next()
                    .map(|win| win.id().clone())
            })
        else {
            return false;
        };

        self.activate_window(&id);
        true
    }

    fn focus_in_stable_container_order(&mut self, descending: bool) -> bool {
        if self.containers.len() <= 1 {
            return false;
        }

        let Some(active_idx) = self.active_container_idx() else {
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

        let Some(id) = self.containers[target_idx]
            .tree
            .focused_window()
            .map(|win| win.id().clone())
            .or_else(|| {
                self.containers[target_idx]
                    .tree
                    .all_windows()
                    .into_iter()
                    .next()
                    .map(|win| win.id().clone())
            })
        else {
            return false;
        };

        self.activate_window(&id);
        true
    }

    fn should_cycle_top_level_stable_order(&self) -> bool {
        self.containers.len() > 1
            && self
                .active_container_idx()
                .is_some_and(|idx| self.has_implicit_single_leaf_root(idx))
            && self
                .containers
                .iter()
                .enumerate()
                .all(|(idx, _)| self.has_implicit_single_leaf_root(idx))
    }

    pub fn focus_left(&mut self) -> bool {
        if self.focus_within_active_container(Direction::Left, true) {
            return true;
        }
        if self.should_cycle_top_level_stable_order() {
            return self.focus_in_stable_container_order(true);
        }
        self.focus_in_stack_order(1) || self.focus_directional(|focus, other| focus.x - other.x)
    }

    pub fn focus_left_no_wrap(&mut self) -> bool {
        if self.focus_within_active_container(Direction::Left, false) {
            return true;
        }
        self.focus_directional(|focus, other| focus.x - other.x)
    }

    pub fn focus_window_by_id(&mut self, id: &W::Id) -> bool {
        let Some(idx) = self.idx_of(id) else {
            return false;
        };

        self.containers[idx].wrapper_selected = false;
        self.containers[idx].tree.clear_selection();
        let _ = self.containers[idx].tree.focus_window_by_id(id);
        self.active_window_id = Some(id.clone());
        true
    }

    pub fn focus_right(&mut self) -> bool {
        if self.focus_within_active_container(Direction::Right, true) {
            return true;
        }
        if self.should_cycle_top_level_stable_order() {
            return self.focus_in_stable_container_order(false);
        }
        self.focus_in_stack_order(-1) || self.focus_directional(|focus, other| other.x - focus.x)
    }

    pub fn focus_right_no_wrap(&mut self) -> bool {
        if self.focus_within_active_container(Direction::Right, false) {
            return true;
        }
        self.focus_directional(|focus, other| other.x - focus.x)
    }

    pub fn focus_up(&mut self) -> bool {
        if self.focus_within_active_container(Direction::Up, true) {
            return true;
        }
        self.focus_directional(|focus, other| focus.y - other.y)
    }

    pub fn focus_up_no_wrap(&mut self) -> bool {
        if self.focus_within_active_container(Direction::Up, false) {
            return true;
        }
        self.focus_directional(|focus, other| focus.y - other.y)
    }

    pub fn focus_down(&mut self) -> bool {
        if self.focus_within_active_container(Direction::Down, true) {
            return true;
        }
        self.focus_directional(|focus, other| other.y - focus.y)
    }

    pub fn focus_down_no_wrap(&mut self) -> bool {
        if self.focus_within_active_container(Direction::Down, false) {
            return true;
        }
        self.focus_directional(|focus, other| other.y - focus.y)
    }

    pub fn focus_leftmost(&mut self) {
        let result = self
            .tiles_with_offsets_visible()
            .min_by(|(_, pos_a), (_, pos_b)| f64::total_cmp(&pos_a.x, &pos_b.x));
        if let Some((tile, _)) = result {
            let id = tile.window().id().clone();
            self.activate_window(&id);
        }
    }

    pub fn focus_rightmost(&mut self) {
        let result = self
            .tiles_with_offsets_visible()
            .max_by(|(_, pos_a), (_, pos_b)| f64::total_cmp(&pos_a.x, &pos_b.x));
        if let Some((tile, _)) = result {
            let id = tile.window().id().clone();
            self.activate_window(&id);
        }
    }

    pub fn focus_topmost(&mut self) {
        let result = self
            .tiles_with_offsets_visible()
            .min_by(|(_, pos_a), (_, pos_b)| f64::total_cmp(&pos_a.y, &pos_b.y));
        if let Some((tile, _)) = result {
            let id = tile.window().id().clone();
            self.activate_window(&id);
        }
    }

    pub fn focus_bottommost(&mut self) {
        let result = self
            .tiles_with_offsets_visible()
            .max_by(|(_, pos_a), (_, pos_b)| f64::total_cmp(&pos_a.y, &pos_b.y));
        if let Some((tile, _)) = result {
            let id = tile.window().id().clone();
            self.activate_window(&id);
        }
    }

    pub fn focus_parent(&mut self) -> bool {
        let Some(idx) = self.active_container_idx() else {
            return false;
        };
        let container = &mut self.containers[idx];
        if container.wrapper_selected {
            let root_child_count = container
                .tree
                .root_info()
                .map(|(_, _, count)| count)
                .unwrap_or(0);
            let root_meaningful = container.tree.root_is_meaningful_parent().unwrap_or(false);
            let preserve_on_single = container
                .tree
                .root_container()
                .is_some_and(|container| container.preserve_on_single())
                && !container.workspace_floated;
            if !root_meaningful || (root_child_count <= 1 && !preserve_on_single) {
                container.wrapper_selected = false;
                container.tree.clear_selection();
                return false;
            }
            // Model rule: once the floating wrapper is selected, the next
            // focus-parent step reaches workspace context while keeping the
            // floating command target available.
            return false;
        }

        let tree = &mut container.tree;
        loop {
            if !tree.select_parent() {
                // No further parent in the container tree. Only expose wrapper selection
                // if root is a meaningful container; otherwise let workspace fallback
                // to tiling focus (sway behavior for redundant single-child wrappers).
                let root_child_count = tree.root_info().map(|(_, _, count)| count).unwrap_or(0);
                let root_meaningful = tree.root_is_meaningful_parent().unwrap_or(false);
                let preserve_on_single = tree
                    .root_container()
                    .is_some_and(|container| container.preserve_on_single())
                    && !container.workspace_floated;
                if root_meaningful && (root_child_count > 1 || preserve_on_single) {
                    container.wrapper_selected = true;
                    return false;
                }
                tree.clear_selection();
                return false;
            }

            let Some(key) = tree.selected_node_key() else {
                return false;
            };
            let is_meaningful = tree.container_is_meaningful_parent(key).unwrap_or(false);
            let selected_child_count = tree
                .container_info(key)
                .map(|(_, _, count)| count)
                .unwrap_or(0);
            if is_meaningful {
                let root_selected = Some(key) == tree.root_node_key();
                let preserve_on_single = if root_selected {
                    tree.root_container()
                        .is_some_and(|container| container.preserve_on_single())
                        && !container.workspace_floated
                } else {
                    false
                };
                if root_selected && selected_child_count <= 1 {
                    // Keep explicitly requested single-child root wrappers selectable
                    // (e.g. after split commands), but ignore implicit redundant wrappers.
                    if !preserve_on_single {
                        tree.clear_selection();
                        return false;
                    }
                }
                container.wrapper_selected = false;
                return true;
            }

            if Some(key) == tree.root_node_key() {
                tree.clear_selection();
                return false;
            }
        }
    }

    pub fn focus_child(&mut self) -> bool {
        let Some(idx) = self.active_container_idx() else {
            return false;
        };
        if self.containers[idx].wrapper_selected {
            self.containers[idx].wrapper_selected = false;
            return true;
        }

        self.containers[idx].tree.select_child()
    }

    fn active_selection_layout(&self, idx: usize) -> Option<Layout> {
        if self.containers[idx].wrapper_selected {
            return self.containers[idx]
                .tree
                .root_container()
                .map(|container| container.layout());
        }

        if self.containers[idx].tree.selected_is_container() {
            let key = self.containers[idx].tree.selected_node_key()?;
            return self.containers[idx]
                .tree
                .container_info(key)
                .map(|(layout, _, _)| layout);
        }

        self.containers[idx].tree.focused_layout()
    }

    fn has_implicit_single_leaf_root(&self, idx: usize) -> bool {
        if self.containers[idx].wrapper_selected
            || self.containers[idx].tree.selected_is_container()
        {
            return false;
        }

        let focus_path = self.containers[idx].tree.focus_path();
        if focus_path.len() != 1 {
            return false;
        }

        let Some(root) = self.containers[idx].tree.root_container() else {
            return false;
        };

        root.child_count() == 1
            && !root.preserve_on_single()
            && matches!(root.layout(), Layout::SplitH | Layout::SplitV)
    }

    fn consume_or_expel_window(&mut self, window: Option<&W::Id>, direction: Direction) {
        if let Some(id) = window {
            if !self.activate_window(id) {
                return;
            }
        }

        let Some(idx) = self.active_container_idx() else {
            return;
        };

        if self.move_tree_command_target(idx, direction) {
            return;
        }

        if self.split_for_active_selection(idx, Layout::SplitV) {
            self.containers[idx].tree.layout();
        }
    }

    pub fn consume_or_expel_window_left(&mut self, window: Option<&W::Id>) {
        self.consume_or_expel_window(window, Direction::Left);
    }

    pub fn consume_or_expel_window_right(&mut self, window: Option<&W::Id>) {
        self.consume_or_expel_window(window, Direction::Right);
    }

    pub fn consume_into_column(&mut self) {
        let Some(idx) = self.active_container_idx() else {
            return;
        };
        if self.split_for_active_selection(idx, Layout::SplitV) {
            self.containers[idx].tree.layout();
        }
    }

    pub fn expel_from_column(&mut self) {
        let Some(idx) = self.active_container_idx() else {
            return;
        };
        if self.split_for_active_selection(idx, Layout::SplitH) {
            self.containers[idx].tree.layout();
        }
    }

    pub fn swap_window_in_direction(&mut self, direction: Direction) {
        let Some(idx) = self.active_container_idx() else {
            return;
        };

        self.move_tree_command_target(idx, direction);
    }

    fn move_tree_command_target(&mut self, idx: usize, direction: Direction) -> bool {
        let target = self.containers[idx]
            .tree
            .command_target(RootPolicy::MaterialContainer);
        let moved = self.containers[idx]
            .tree
            .move_target_in_direction(direction, target);
        if moved {
            self.containers[idx].wrapper_selected = false;
            self.containers[idx].tree.layout();
        }
        moved
    }

    pub fn set_column_display(&mut self, display: ColumnDisplay) {
        let target_layout = match display {
            ColumnDisplay::Normal => Layout::SplitV,
            ColumnDisplay::Tabbed => Layout::Tabbed,
        };

        let Some(idx) = self.active_container_idx() else {
            return;
        };
        if self.set_layout_for_active_selection(idx, target_layout) {
            self.containers[idx].tree.layout();
        }
    }

    pub fn toggle_column_tabbed_display(&mut self) {
        let Some(idx) = self.active_container_idx() else {
            return;
        };
        let target = match self.active_selection_layout(idx) {
            Some(Layout::Tabbed) => Layout::SplitV,
            _ => Layout::Tabbed,
        };
        if self.set_layout_for_active_selection(idx, target) {
            self.containers[idx].tree.layout();
        }
    }

    fn split_for_active_selection(&mut self, idx: usize, layout: Layout) -> bool {
        if self.containers[idx].wrapper_selected {
            return self.containers[idx].tree.split_root_container(layout);
        }

        let target = self.containers[idx]
            .tree
            .command_target(RootPolicy::MaterialContainer);
        self.containers[idx]
            .tree
            .split_target(layout, target, RootPolicy::MaterialContainer)
    }

    fn set_layout_for_active_selection(&mut self, idx: usize, layout: Layout) -> bool {
        if self.containers[idx].wrapper_selected {
            // Top-level floating containers are not layout targets.
            return false;
        }

        if self.containers[idx].tree.selected_is_container() {
            let path = self.containers[idx].tree.selected_path();
            if path.is_empty() {
                // Root floating container has no parent target.
                return false;
            }
            let target = self.containers[idx]
                .tree
                .command_target(RootPolicy::MaterialContainer);
            return self.containers[idx].tree.set_layout_for_target(
                layout,
                target,
                RootPolicy::MaterialContainer,
            );
        }

        // Model rule: layout commands on a standalone floating leaf are no-op.
        if self.active_selection_layout(idx).is_none() || self.has_implicit_single_leaf_root(idx) {
            return false;
        }

        let target = self.containers[idx]
            .tree
            .command_target(RootPolicy::MaterialContainer);
        self.containers[idx].tree.set_layout_for_target(
            layout,
            target,
            RootPolicy::MaterialContainer,
        )
    }

    fn toggle_split_for_active_selection(&mut self, idx: usize) -> bool {
        // Toggling a selected container's split targets its *parent*, matching i3.
        let target_key = if self.containers[idx].wrapper_selected {
            None
        } else if self.containers[idx].tree.selected_is_container() {
            let tree = &self.containers[idx].tree;
            tree.selected_node_key()
                .and_then(|key| tree.parent_of_node(key))
        } else {
            None
        };

        if let Some(key) = target_key {
            if let Some((current, _, _)) = self.containers[idx].tree.container_info(key) {
                let next = match current {
                    Layout::SplitH => Layout::SplitV,
                    Layout::SplitV => Layout::SplitH,
                    Layout::Tabbed | Layout::Stacked => Layout::SplitH,
                };
                if let Some(container) = self.containers[idx].tree.container_mut(key) {
                    container.set_layout_explicit(next);
                    return true;
                }
            }
        }

        // Model rule: layout commands on a standalone floating leaf are no-op.
        if self.active_selection_layout(idx).is_none() || self.has_implicit_single_leaf_root(idx) {
            return false;
        }

        let target = self.containers[idx]
            .tree
            .command_target(RootPolicy::MaterialContainer);
        self.containers[idx]
            .tree
            .toggle_split_for_target(target, RootPolicy::MaterialContainer)
    }

    fn toggle_layout_all_for_active_selection(&mut self, idx: usize) -> bool {
        // Cycling a selected container's layout targets its *parent*, matching i3.
        let target_key = if self.containers[idx].wrapper_selected {
            None
        } else if self.containers[idx].tree.selected_is_container() {
            let tree = &self.containers[idx].tree;
            tree.selected_node_key()
                .and_then(|key| tree.parent_of_node(key))
        } else {
            None
        };

        if let Some(key) = target_key {
            if let Some((current, _, _)) = self.containers[idx].tree.container_info(key) {
                let next = current.next_in_cycle();
                if let Some(container) = self.containers[idx].tree.container_mut(key) {
                    container.set_layout_explicit(next);
                    return true;
                }
            }
        }

        // Model rule: layout commands on a standalone floating leaf are no-op.
        if self.active_selection_layout(idx).is_none() || self.has_implicit_single_leaf_root(idx) {
            return false;
        }

        let target = self.containers[idx]
            .tree
            .command_target(RootPolicy::MaterialContainer);
        self.containers[idx]
            .tree
            .toggle_layout_all_for_target(target, RootPolicy::MaterialContainer)
    }

    pub fn split_horizontal(&mut self) {
        let Some(idx) = self.active_container_idx() else {
            return;
        };
        if self.split_for_active_selection(idx, Layout::SplitH) {
            self.containers[idx].tree.layout();
        }
    }

    pub fn split_vertical(&mut self) {
        let Some(idx) = self.active_container_idx() else {
            return;
        };
        if self.split_for_active_selection(idx, Layout::SplitV) {
            self.containers[idx].tree.layout();
        }
    }

    pub fn set_layout_mode(&mut self, layout: Layout) {
        let Some(idx) = self.active_container_idx() else {
            return;
        };
        if self.set_layout_for_active_selection(idx, layout) {
            self.containers[idx].tree.layout();
        }
    }

    pub fn toggle_split_layout(&mut self) {
        let Some(idx) = self.active_container_idx() else {
            return;
        };
        if self.toggle_split_for_active_selection(idx) {
            self.containers[idx].tree.layout();
        }
    }

    pub fn toggle_layout_all(&mut self) {
        let Some(idx) = self.active_container_idx() else {
            return;
        };
        if self.toggle_layout_all_for_active_selection(idx) {
            self.containers[idx].tree.layout();
        }
    }

    fn move_container_to(&mut self, idx: usize, new_pos: Point<f64, Logical>, animate: bool) {
        if animate {
            self.move_container_and_animate(idx, new_pos);
        } else {
            self.containers[idx].data.set_logical_pos(new_pos);
        }

        self.interactive_resize_end(None);
    }

    fn move_by(&mut self, amount: Point<f64, Logical>) {
        let Some(active_id) = &self.active_window_id else {
            return;
        };
        if self.fullscreen_window.as_ref() == Some(active_id) {
            return;
        }
        let idx = self.idx_of(active_id).unwrap();

        let new_pos = self.containers[idx].data.logical_pos + amount;
        self.move_container_to(idx, new_pos, true)
    }

    pub fn move_left(&mut self) {
        self.move_by(Point::from((-DIRECTIONAL_MOVE_PX, 0.)));
    }

    pub fn move_right(&mut self) {
        self.move_by(Point::from((DIRECTIONAL_MOVE_PX, 0.)));
    }

    pub fn move_up(&mut self) {
        self.move_by(Point::from((0., -DIRECTIONAL_MOVE_PX)));
    }

    pub fn move_down(&mut self) {
        self.move_by(Point::from((0., DIRECTIONAL_MOVE_PX)));
    }

    pub fn move_window(
        &mut self,
        id: Option<&W::Id>,
        x: PositionChange,
        y: PositionChange,
        animate: bool,
    ) {
        let Some(id) = self.resolve_target_id(id) else {
            return;
        };
        if self.fullscreen_window.as_ref() == Some(&id) {
            return;
        }
        let idx = self.idx_of(&id).unwrap();

        let mut pos = self.containers[idx].data.logical_pos;

        let available_width = self.working_area.size.w;
        let available_height = self.working_area.size.h;
        let working_area_loc = self.working_area.loc;

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

        self.move_container_to(idx, pos, animate);
    }

    pub fn center_window(&mut self, id: Option<&W::Id>) {
        let Some(id) = id.or(self.active_window_id.as_ref()).cloned() else {
            return;
        };
        if self.fullscreen_window.as_ref() == Some(&id) {
            return;
        }
        let idx = self.idx_of(&id).unwrap();

        let new_pos =
            center_preferring_top_left_in_area(self.working_area, self.containers[idx].data.size);
        self.move_container_to(idx, new_pos, true);
    }

    pub fn descendants_added(&mut self, id: &W::Id) -> bool {
        let Some(idx) = self.idx_of(id) else {
            return false;
        };

        self.bring_up_descendants_of(idx);
        true
    }

    pub fn update_window(&mut self, id: &W::Id, serial: Option<Serial>) -> bool {
        let Some(container_idx) = self.idx_of(id) else {
            return false;
        };

        {
            let container = &mut self.containers[container_idx];
            let Some(key) = container.tree.window_key(id) else {
                return false;
            };
            let Some(tile) = container.tree.get_tile_mut(key) else {
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

        let container = &mut self.containers[container_idx];
        container.tree.layout();

        if container.tree.window_count() == 1 {
            let Some(key) = container.tree.window_key(id) else {
                return true;
            };
            let Some(tile) = container.tree.get_tile(key) else {
                return true;
            };
            let tile_size = tile.tile_size();
            container.data.set_size(tile_size);
        }

        true
    }

    fn render_elements<R: NiriRenderer>(
        &self,
        mut ctx: RenderCtx<R>,
        xray_pos: XrayPos,
        view_rect: Rectangle<f64, Logical>,
        focus_ring: bool,
    ) -> Vec<FloatingSpaceRenderElement<R>> {
        let tile_count = self.tiles().count();
        let estimated_capacity = tile_count * 4 + self.closing_windows.len() + tile_count / 2;
        let mut elements = Vec::with_capacity(estimated_capacity);
        let scale = Scale::from(self.scale);

        // Draw the closing windows on top of the other windows.
        //
        // FIXME: I guess this should rather preserve the stacking order when the window is closed.
        for closing in self.closing_windows.iter().rev() {
            let elem = closing.render(ctx.as_gles(), view_rect, scale);
            elements.push(elem.into());
        }

        let active = self.active_window_id.clone();
        let selection_is_container = self
            .active_container_idx()
            .is_some_and(|idx| self.selected_is_container_in(idx));

        // Like tiling, push container selection before the regular window
        // contents so it stays visually on top after the global reverse-order
        // composition pass in the renderer.
        if (focus_ring || self.is_active)
            && selection_is_container
            && self.fullscreen_window.is_none()
        {
            if let Some(idx) = self.active_container_idx() {
                if let Some((_, local_rect, _)) = self
                    .selected_key_in(idx)
                    .and_then(|key| self.containers[idx].tree.container_info(key))
                {
                    let rect = Rectangle::new(
                        self.containers[idx].data.logical_pos + local_rect.loc,
                        local_rect.size,
                    );
                    render_container_selection(
                        ctx.renderer,
                        rect,
                        view_rect,
                        self.scale,
                        self.is_active,
                        self.options.layout.focus_ring,
                        self.options.layout.border,
                        ContainerSelectionStyle::Floating,
                        &mut |elem| {
                            elements.push(FloatingSpaceRenderElement::ContainerSelection(elem))
                        },
                    );
                }
            }
        }

        if !self.options.layout.tab_bar.off && self.fullscreen_window.is_none() {
            let mut cache = self.tab_bar_cache.borrow_mut();
            let mut next_cache = self.tab_bar_cache_alt.borrow_mut();
            next_cache.clear();
            let gles = ctx.renderer.as_gles_renderer();
            let tab_bar_config = self.options.layout.tab_bar.clone();
            let is_active_workspace = self.is_active;
            let gap = self.container_gap();
            let target = ctx.target;

            for container in &self.containers {
                for info in container.tree.tab_bar_layouts() {
                    let mut info = info.clone();
                    if gap > 0.0 && info.path.is_empty() {
                        info.rect.loc.x -= gap;
                        info.rect.loc.y -= gap;
                        info.rect.size.w = (info.rect.size.w + gap * 2.0).max(0.0);
                    }
                    info.rect.loc += container.data.logical_pos;
                    let key = (container.id, info.path.clone());
                    let state = tab_bar_state_from_info(
                        &info,
                        &tab_bar_config,
                        is_active_workspace,
                        self.scale,
                        target,
                    );
                    let (buffer, tab_widths_px) = match cache.get(&key) {
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
                    elements.push(FloatingSpaceRenderElement::TabBar(
                        PrimaryGpuTextureRenderElement(elem),
                    ));

                    next_cache.insert(
                        key,
                        TabBarCacheEntry {
                            state,
                            buffer,
                            tab_widths_px,
                        },
                    );
                }
            }

            std::mem::swap(&mut *cache, &mut *next_cache);
        } else {
            self.tab_bar_cache.borrow_mut().clear();
        }

        if let Some(fullscreen_id) = &self.fullscreen_window {
            // Only render the fullscreen tile at (0, 0).
            if let Some(tile) = self.tiles().find(|t| t.window().id() == fullscreen_id) {
                let is_focused = self.is_active;
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
            for (tile, tile_pos) in self.tiles_with_render_positions() {
                // Skip tiles entirely outside the viewport (culling)
                let tile_rect = Rectangle::new(tile_pos, tile.tile_size());
                if !tile_rect.overlaps(view_rect) {
                    continue;
                }

                let is_focused = self.is_active
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
        ctx: RenderCtx<R>,
        xray_pos: XrayPos,
        view_rect: Rectangle<f64, Logical>,
        focus_ring: bool,
        push: &mut dyn FnMut(FloatingSpaceRenderElement<R>),
    ) {
        for elem in self.render_elements(ctx, xray_pos, view_rect, focus_ring) {
            push(elem);
        }
    }

    pub fn interactive_resize_begin(&mut self, window: W::Id, edges: ResizeEdge) -> bool {
        if self.interactive_resize.is_some() {
            return false;
        }

        let Some(idx) = self.idx_of(&window) else {
            return false;
        };

        let container = &self.containers[idx];
        let Some(key) = container.tree.window_key(&window) else {
            return false;
        };
        let Some(tile) = container.tree.get_tile(key) else {
            return false;
        };

        let original_window_size = tile.window_size();
        let original_window_pos = container.data.logical_pos;
        let original_container_size = container.data.size;
        let resize_container_edges = Self::display_layouts(&container.tree)
            .iter()
            .find(|info| info.key == key)
            .map(|info| Self::external_edges_for_rect(container.data.size, info.rect, edges))
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
        window: &W::Id,
        delta: Point<f64, Logical>,
    ) -> bool {
        let Some(idx) = self.idx_of(window) else {
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
            let container = &self.containers[idx];
            let Some(tile) = container
                .tree
                .window_key(window)
                .and_then(|key| container.tree.get_tile(key))
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
                    idx,
                    SizeChange::SetFixed(target_width),
                    true,
                    false,
                );
            } else {
                self.set_window_width(Some(window), SizeChange::SetFixed(target_width), false);
            }
        }

        if edges.intersects(ResizeEdge::TOP_BOTTOM) {
            if resize_container_v {
                self.resize_container_dimension(
                    idx,
                    SizeChange::SetFixed(target_height),
                    false,
                    false,
                );
            } else {
                self.set_window_height(Some(window), SizeChange::SetFixed(target_height), false);
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
                self.containers[idx]
                    .data
                    .set_logical_pos(original_pos + move_pos);
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

    pub fn refresh(&mut self, is_active: bool, is_focused: bool) {
        let active = self.active_window_id.clone();
        let deactivate_unfocused = self.options.deactivate_unfocused_windows;
        let disable_resize_throttling = self.options.disable_resize_throttling;
        let border_base = self.options.layout.border;
        let working_area_size = self.working_area.size;
        let resize_target = self.interactive_resize.as_ref().and_then(|resize| {
            let idx = self.idx_of(&resize.window)?;
            let mut ids = Vec::new();
            for tile in self.containers[idx].tree.all_tiles() {
                ids.push(tile.window().id().clone());
            }
            Some((resize.data, ids))
        });
        for tile in self.tiles_mut() {
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
        pos: Point<f64, Logical>,
        size: Size<f64, Logical>,
    ) -> Point<f64, Logical> {
        let mut rect = Rectangle::new(pos, size);
        clamp_preferring_top_left_in_area(self.working_area, &mut rect);
        rect.loc
    }

    pub fn scale_by_working_area(&self, pos: Point<f64, SizeFrac>) -> Point<f64, Logical> {
        FloatingContainerData::scale_by_working_area(self.working_area, pos)
    }

    pub fn logical_to_size_frac(&self, logical_pos: Point<f64, Logical>) -> Point<f64, SizeFrac> {
        FloatingContainerData::logical_to_size_frac_in_working_area(self.working_area, logical_pos)
    }

    fn move_container_and_animate(&mut self, idx: usize, new_pos: Point<f64, Logical>) {
        // Moves up to this logical pixel distance are not animated.
        const ANIMATION_THRESHOLD_SQ: f64 = 10. * 10.;

        let container = &mut self.containers[idx];
        let prev_pos = container.data.logical_pos;
        container.data.set_logical_pos(new_pos);
        let new_pos = container.data.logical_pos;

        let diff = prev_pos - new_pos;
        if diff.x * diff.x + diff.y * diff.y > ANIMATION_THRESHOLD_SQ {
            let delta = prev_pos - new_pos;
            for tile in container.tree.all_tiles_mut() {
                tile.animate_move_from(delta);
            }
        }
    }

    pub fn new_window_size(
        &self,
        width: Option<PresetSize>,
        height: Option<PresetSize>,
        rules: &ResolvedWindowRules,
    ) -> Size<i32, Logical> {
        let border = self.options.layout.border.merged_with(&rules.border);

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

        let width = resolve(width, self.working_area.size.w);
        let height = resolve(height, self.working_area.size.h);

        Size::from((width, height))
    }

    pub fn stored_or_default_tile_pos(&self, tile: &Tile<W>) -> Option<Point<f64, Logical>> {
        if tile.is_scratchpad() && tile.floating_pos.is_none() {
            return None;
        }

        let pos = tile.floating_pos.map(|pos| self.scale_by_working_area(pos));
        pos.or_else(|| {
            tile.window().rules().default_floating_position.map(|pos| {
                let relative_to = pos.relative_to;
                let size = tile.tile_size();
                let area = self.working_area;

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

                pos + self.working_area.loc
            })
        })
    }

    #[cfg(test)]
    pub fn view_size(&self) -> Size<f64, Logical> {
        self.view_size
    }

    pub fn working_area(&self) -> Rectangle<f64, Logical> {
        self.working_area
    }

    #[cfg(test)]
    pub fn scale(&self) -> f64 {
        self.scale
    }

    #[cfg(test)]
    pub fn clock(&self) -> &Clock {
        &self.clock
    }

    #[cfg(test)]
    pub fn options(&self) -> &Rc<Options> {
        &self.options
    }

    #[cfg(test)]
    pub fn wrapper_selected_for_window(&self, id: &W::Id) -> bool {
        self.idx_of(id)
            .is_some_and(|idx| self.containers[idx].wrapper_selected)
    }

    #[cfg(test)]
    pub fn root_layout_for_window(&self, id: &W::Id) -> Option<Layout> {
        let idx = self.idx_of(id)?;
        self.containers[idx]
            .tree
            .root_container()
            .map(|container| container.layout())
    }

    #[cfg(test)]
    pub fn debug_tree_for_window(&self, id: &W::Id) -> Option<String>
    where
        W::Id: std::fmt::Display,
    {
        let idx = self.idx_of(id)?;
        Some(self.containers[idx].tree.debug_tree())
    }

    #[cfg(test)]
    pub fn verify_invariants(&self) {
        assert!(self.scale > 0.);
        assert!(self.scale.is_finite());
        for container in &self.containers {
            use crate::layout::SizingMode;

            container.data.verify_invariants();
            container.tree.verify_invariants();

            for tile in container.tree.all_tiles() {
                assert!(Rc::ptr_eq(&self.options, &tile.options));
                assert_eq!(self.view_size, tile.view_size());
                assert_eq!(self.clock, tile.clock);
                assert_eq!(self.scale, tile.scale());
                tile.verify_invariants();

                if let Some(idx) = tile.floating_preset_width_idx {
                    assert!(idx < self.options.layout.preset_column_widths.len());
                }
                if let Some(idx) = tile.floating_preset_height_idx {
                    assert!(idx < self.options.layout.preset_window_heights.len());
                }

                let is_fullscreen_tile = self
                    .fullscreen_window
                    .as_ref()
                    .is_some_and(|id| id == tile.window().id());
                if !is_fullscreen_tile {
                    assert_eq!(
                        tile.window().pending_sizing_mode(),
                        SizingMode::Normal,
                        "floating windows cannot be maximized or fullscreen"
                    );
                }
            }
        }

        if let Some(id) = &self.active_window_id {
            assert!(!self.containers.is_empty());
            assert!(self.contains(id), "active window must be present in tiles");
        } else {
            assert!(self.containers.is_empty());
        }

        if let Some(resize) = &self.interactive_resize {
            assert!(
                self.contains(&resize.window),
                "interactive resize window must be present in tiles"
            );
        }
    }
}

impl FloatingSpace<Mapped> {
    pub(crate) fn layout_tree_nodes(&self) -> Vec<LayoutTreeNode> {
        self.containers
            .iter()
            .enumerate()
            .filter_map(|(idx, container)| {
                let focused_key = if container.wrapper_selected {
                    container.tree.root_node_key()
                } else {
                    container.tree.selected_node_key()
                };
                let mut path = vec![idx];
                container.tree.layout_tree_with_context(
                    focused_key,
                    &mut path,
                    1,
                    container.data.logical_pos,
                    true,
                )
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
