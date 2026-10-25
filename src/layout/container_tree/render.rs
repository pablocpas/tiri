//! Drawing the tiled side of a workspace, and the caches drawing keeps.
//!
//! The model above this module never needs a renderer. What is here reads the tree and
//! produces render elements; the caches it keeps are keyed by node and dropped with their node,
//! which is why they live on the tree rather than beside it.

use log::warn;
use smithay::backend::renderer::gles::GlesRenderer;

use crate::render_helpers::primary_gpu_texture::PrimaryGpuTextureRenderElement;
use crate::render_helpers::renderer::NiriRenderer;
use crate::render_helpers::texture::TextureRenderElement;
use crate::render_helpers::{RenderCtx, RenderTarget};

use super::*;

/// Render caches derived from the shared tree.
///
/// Keeping these together makes their ownership explicit without giving either the tiled or
/// floating side a second copy. None of this state is part of the container topology.
#[derive(Debug)]
pub(in crate::layout) struct ContainerRenderState {
    /// Tab bars already drawn, for both sides of the workspace.
    ///
    /// One cache, because a node key is unique across the tree; two would have to agree on
    /// which side a container is on, and a container changes sides. Entries are dropped when
    /// their container leaves the tree, rather than when a frame does not redraw them: a
    /// fullscreen draws no bars at all and is not a reason to forget every one of them.
    pub(in crate::layout) tab_bar_cache: RefCell<HashMap<NodeKey, TabBarCacheEntry>>,
    /// Per-workspace overview buffers.
    ///
    /// The overview draws every workspace in one frame, each into its own texture, so a
    /// single offscreen would have every thumbnail overwrite the last.
    pub(in crate::layout) overview: HashMap<NodeKey, WorkspaceOverview>,
    /// Copy-only state projection reused while mutably visiting tiles.
    pub(in crate::layout) state_layout_scratch: Vec<LeafFrameInfo>,
    /// Displayed geometry can differ from pending state while a transaction is open.
    pub(in crate::layout) render_layout_scratch: Vec<LeafFrameInfo>,
    /// Edge projection parallel to `render_layout_scratch`.
    pub(in crate::layout) render_edges_scratch:
        Vec<(FocusRingEdges, Option<FocusRingIndicatorEdge>)>,
}

/// One workspace's overview thumbnail: its own texture and the ground under it.
#[derive(Debug)]
pub(in crate::layout) struct WorkspaceOverview {
    offscreen: OffscreenBuffer,
    pub(in crate::layout) background: SolidColorBuffer,
}

impl WorkspaceOverview {
    pub(in crate::layout) fn new(view_size: Size<f64, Logical>, background_color: Color) -> Self {
        Self {
            offscreen: OffscreenBuffer::default(),
            background: SolidColorBuffer::new(view_size, background_color),
        }
    }
}

impl ContainerRenderState {
    pub(in crate::layout) fn new() -> Self {
        Self {
            tab_bar_cache: RefCell::new(HashMap::new()),
            overview: HashMap::new(),
            state_layout_scratch: Vec::new(),
            render_layout_scratch: Vec::new(),
            render_edges_scratch: Vec::new(),
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

impl<W: LayoutElement> ContainerTree<W> {
    fn render_elements<R: NiriRenderer>(
        &self,
        workspace: NodeKey,
        closing: &[ClosingWindow],
        mut ctx: RenderCtx<R>,
        xray_pos: XrayPos,
        params: BranchRenderParams,
    ) -> Vec<ContainerTreeRenderElement<R>> {
        let BranchRenderParams {
            has_focus: tiling_has_focus,
            focus_ring: tiling_focus_ring,
            layer,
        } = params;
        // Pre-allocate: ~4 elements per tile + closing windows + tab bars
        let tile_count = self.window_count();
        let estimated_capacity = tile_count * 4 + closing.len() + tile_count / 2;
        let mut elements = Vec::with_capacity(estimated_capacity);
        let mut active_elements = Vec::with_capacity(8);
        let scale = Scale::from(self.scale(workspace));
        let focused_key = self.focused_key(workspace);
        let selection_is_layout_parent = self.selected_layout_parent_in_tiling(workspace);
        let selection_is_active = tiling_has_focus || self.workspace_is_selected(workspace);
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

                let is_focused = tiling_has_focus
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
            let tab_bar_config = self.effective_tab_bar_config(workspace);
            let gles = ctx.renderer.as_gles_renderer();
            for info in self.tab_bar_layouts(workspace) {
                if fullscreen
                    .container
                    .is_some_and(|scope| !self.is_descendant(info.key, scope))
                {
                    continue;
                }
                if let Some(elem) = self.tab_bar_element(
                    gles,
                    ctx.target,
                    &info,
                    &tab_bar_config,
                    tiling_has_focus,
                    self.scale(workspace),
                ) {
                    elements.push(ContainerTreeRenderElement::TabBar(elem));
                }
            }
        }

        elements
    }

    pub(in crate::layout) fn render<R: NiriRenderer>(
        &self,
        workspace: NodeKey,
        closing: &[ClosingWindow],
        ctx: RenderCtx<R>,
        xray_pos: XrayPos,
        params: BranchRenderParams,
        push: &mut dyn FnMut(ContainerTreeRenderElement<R>),
    ) {
        for elem in self.render_elements(workspace, closing, ctx, xray_pos, params) {
            push(elem);
        }
    }

    pub(in crate::layout) fn render_as_offscreen(
        &self,
        workspace: NodeKey,
        closing: &[ClosingWindow],
        renderer: &mut GlesRenderer,
        target: RenderTarget,
        params: BranchRenderParams,
    ) -> Option<OffscreenRenderElement> {
        for tile in self.tiled_tiles(workspace) {
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
            BranchRenderParams {
                layer: RenderLayer::Normal,
                ..params
            },
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
                for tile in self.tiled_tiles(workspace) {
                    tile.window().set_offscreen_data(Some(data.clone()));
                }

                elem
            })
            .map_err(|err| warn!("error rendering tiled branch to offscreen: {err:?}"))
            .ok()
    }

    // LayoutMode operations using ContainerTree
    pub(in crate::layout) fn tab_bar_element(
        &self,
        renderer: &mut GlesRenderer,
        target: RenderTarget,
        info: &TabBarInfo,
        config: &TabBar,
        is_active: bool,
        scale: f64,
    ) -> Option<PrimaryGpuTextureRenderElement> {
        let state = tab_bar_state_from_info(info, config, is_active, scale, target);
        let mut cache = self.render.tab_bar_cache.borrow_mut();
        let buffer = match cache.get(&info.key) {
            Some(entry) if entry.state == state => entry.buffer.clone(),
            _ => {
                let output = match render_tab_bar(
                    renderer,
                    config,
                    info.layout,
                    info.rect,
                    info.row_height,
                    &info.tabs,
                    is_active,
                    target,
                    scale,
                ) {
                    Ok(output) => output,
                    Err(err) => {
                        warn!("tab bar render failed: {err}");
                        return None;
                    }
                };
                let buffer = output.buffer.clone();
                cache.insert(
                    info.key,
                    TabBarCacheEntry {
                        state,
                        buffer: output.buffer,
                        tab_widths_px: output.tab_widths_px,
                    },
                );
                buffer
            }
        };

        let scale = Scale::from(scale);
        let location = info
            .rect
            .loc
            .to_physical_precise_round(scale)
            .to_logical(scale);
        Some(PrimaryGpuTextureRenderElement(
            TextureRenderElement::from_texture_buffer(
                buffer,
                location,
                1.0,
                None,
                None,
                Kind::Unspecified,
            ),
        ))
    }

    pub(in crate::layout) fn update_render_elements(
        &mut self,
        workspace: NodeKey,
        is_active: bool,
        resize: &Option<InteractiveResizeState<W>>,
    ) {
        let _span = tracy_client::span!("ContainerTree::update_render_elements");
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
                let tree = &*self;
                state_layouts.extend(
                    tree.leaf_layouts_in(workspace)
                        .map(|info| LeafFrameInfo::new(tree.branch_root(info.key), info)),
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

        let active_children = self.active_children();
        for (info, (edges, indicator_edge)) in render_layouts
            .iter()
            .copied()
            .zip(render_edges.iter().copied())
        {
            let is_in_fullscreen_container =
                fullscreen_container.is_some_and(|scope| self.is_descendant(info.key, scope));
            // Asked before the tile is borrowed mutably below.
            let is_focus_head = active_children.is_focus_head(self, info.key);
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

    pub(in crate::layout) fn start_close_animation_for_window<R: NiriRenderer>(
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
}
