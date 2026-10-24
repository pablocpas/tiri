use std::cmp::max;
use std::rc::Rc;

use smithay::backend::renderer::element::Kind;
use smithay::backend::renderer::gles::GlesRenderer;
use smithay::desktop::{layer_map_for_output, Window};
use smithay::input::pointer::CursorIcon;
use smithay::output::Output;
use smithay::reexports::wayland_protocols::xdg::shell::server::xdg_toplevel;
use smithay::utils::{Logical, Point, Rectangle, Serial, Size, Transform};
use smithay::wayland::compositor::with_states;
use smithay::wayland::shell::xdg::SurfaceCachedState;
use tiri_config::utils::MergeWith as _;
use tiri_config::{CornerRadius, PresetSize, Struts, Workspace as WorkspaceConfig};
use tiri_ipc::{ColumnDisplay, LayoutTreeNode, PositionChange, SizeChange};

use super::container::{
    Direction, InactiveTilingReference, IncomingView, InsertParentInfo, Layout, LayoutLayer,
    NewView, NodeKey, SwapEffects, WorkspaceCommandTarget,
};
use super::container_tree::{ContainerTree, ContainerTreeRenderElement, RootTilingSubtree};
use super::floating::{
    compute_toplevel_bounds, FloatingRenderElement, FloatingResizeResult, FloatingRuntime,
};
use super::shadow::Shadow;
use super::tile::{Tile, TileRenderSnapshot};
use super::{
    ActivateWindow, BranchRenderParams, HitType, InsertPosition, InteractiveResizeData,
    LayoutCycleEntry, LayoutElement, Options, ResizeHit, ResizeRequest, SizeFrac, ViewTransfer,
};
use crate::layout::RenderLayer;
use crate::niri_render_elements;
use crate::render_helpers::offscreen::OffscreenRenderElement;
use crate::render_helpers::renderer::NiriRenderer;
use crate::render_helpers::shadow::ShadowRenderElement;
use crate::render_helpers::solid_color::{SolidColorBuffer, SolidColorRenderElement};
use crate::render_helpers::xray::{Xray, XrayPos};
use crate::render_helpers::{RenderCtx, RenderTarget};
use crate::utils::transaction::{Transaction, TransactionBlocker};
use crate::utils::{
    center_preferring_top_left_in_area, ensure_min_max_size, ensure_min_max_size_maybe_zero,
    output_size, send_scale_transform, ResizeEdge,
};
use crate::window::ResolvedWindowRules;

#[cfg(test)]
mod test_view;
#[cfg(test)]
use test_view::{FloatingTestView, WorkspaceTreeView};
mod runtime;
use runtime::{WorkspaceLayerState, WorkspaceRuntime};
mod state;
pub use state::{OutputId, WorkspaceId};
pub(super) use state::{WorkspaceIdentity, WorkspaceLifetime};
mod transfer;
use transfer::{FloatTransfer, FloatingTransfer, UnfloatTransfer};

#[derive(Debug)]
pub struct Workspace<W: LayoutElement> {
    /// This workspace's root node in the layout's tree.
    workspace: NodeKey,

    /// Interaction, animation and render scratch that do not belong to tree topology.
    ///
    /// Structural state for both branches lives exclusively in the workspace node held by
    /// `ContainerTree`; this runtime never owns roots, geometry, stacking or fullscreen.
    runtime: WorkspaceRuntime<W>,

    /// The original output of this workspace.
    ///
    /// Most of the time this will be the workspace's current output, however, after an output
    /// disconnection, it may remain pointing to the disconnected output.
    pub(super) original_output: OutputId,

    /// Current output of this workspace.
    output: Option<Output>,

    /// Latest known output scale for this workspace.
    ///
    /// This should be set from the current workspace output, or, if all outputs have been
    /// disconnected, preserved until a new output is connected.
    scale: smithay::output::Scale,

    /// Latest known output transform for this workspace.
    ///
    /// This should be set from the current workspace output, or, if all outputs have been
    /// disconnected, preserved until a new output is connected.
    transform: Transform,

    /// This workspace's shadow in the overview.
    shadow: Shadow,

    /// This workspace's background.
    background_buffer: SolidColorBuffer,

    /// Configurable properties of the layout as received from the parent monitor.
    ///
    /// The merged, scale-adjusted options this workspace is actually laid out with live on its
    /// tree node, together with the box — see [`ContainerTree::update_config`]. Only what the
    /// tree has no use for stays here: the monitor's unmerged options, which a scale change
    /// re-derives from, the integer half of the scale, and the transform.
    pub(super) base_options: Rc<Options>,

    /// Stable identity of this workspace.
    identity: WorkspaceIdentity,
    /// Whether the workspace should survive when empty and inactive.
    lifetime: WorkspaceLifetime,

    /// Layout config overrides for this workspace.
    layout_config: Option<tiri_config::LayoutPart>,

    /// Unique ID of this workspace.
    id: WorkspaceId,
}

niri_render_elements! {
    WorkspaceRenderElement<R> => {
        Tiling = ContainerTreeRenderElement<R>,
        Floating = FloatingRenderElement<R>,
        Offscreen = OffscreenRenderElement,
    }
}

#[derive(Debug)]
pub(super) struct InteractiveResize<W: LayoutElement> {
    pub window: W::Id,
    pub original_window_size: Size<f64, Logical>,
    pub original_window_pos: Option<Point<f64, Logical>>,
    pub original_container_size: Size<f64, Logical>,
    pub resize_container_edges: ResizeEdge,
    pub data: InteractiveResizeData,
}

/// Resolved width or height in logical pixels.
#[derive(Debug, Clone, Copy)]
pub enum ResolvedSize {
    /// Size of the tile including borders.
    Tile(f64),
    /// Size of the window excluding borders.
    Window(f64),
}

/// Where to put a newly added window.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub enum WorkspaceAddWindowTarget<'a, W: LayoutElement> {
    /// No particular preference.
    #[default]
    Auto,
    /// At this index among the tiling root's children.
    AtRootIndex(usize),
    /// Next to this existing window.
    NextTo(&'a W::Id),
}

fn external_resize_cursor_icon(edges: ResizeEdge) -> CursorIcon {
    if edges.contains(ResizeEdge::TOP) && edges.contains(ResizeEdge::LEFT) {
        return CursorIcon::NwResize;
    }
    if edges.contains(ResizeEdge::TOP) && edges.contains(ResizeEdge::RIGHT) {
        return CursorIcon::NeResize;
    }
    if edges.contains(ResizeEdge::BOTTOM) && edges.contains(ResizeEdge::RIGHT) {
        return CursorIcon::SeResize;
    }
    if edges.contains(ResizeEdge::BOTTOM) && edges.contains(ResizeEdge::LEFT) {
        return CursorIcon::SwResize;
    }
    if edges.contains(ResizeEdge::LEFT) {
        return CursorIcon::WResize;
    }
    if edges.contains(ResizeEdge::RIGHT) {
        return CursorIcon::EResize;
    }
    if edges.contains(ResizeEdge::TOP) {
        return CursorIcon::NResize;
    }
    if edges.contains(ResizeEdge::BOTTOM) {
        return CursorIcon::SResize;
    }

    CursorIcon::Default
}

impl<W: LayoutElement> Workspace<W> {
    /// Discard this workspace together with its root in the layout-wide tree.
    ///
    /// Dropping the Rust handle alone is not enough now that the tree outlives every
    /// workspace. Keeping the two actions together makes deletion sites explicit and avoids
    /// leaving an unreachable root (and its windows) in the shared tree.
    pub(super) fn remove_from_tree(self, containers: &mut ContainerTree<W>) {
        assert!(containers.remove_workspace(self.workspace));
    }

    fn numeric_identity_from_name(name: &str) -> Option<u32> {
        let (number, rest) = match name.split_once(':') {
            Some((number, rest)) if !rest.is_empty() => (number, Some(rest)),
            Some(_) => return None,
            None => (name, None),
        };

        let number = number.parse::<u32>().ok()?;
        (number.to_string() == name || rest.is_some()).then_some(number)
    }

    fn identity_from_config(config: Option<&WorkspaceConfig>) -> WorkspaceIdentity {
        let Some(config) = config else {
            return WorkspaceIdentity::Anonymous;
        };

        let name = config.name.0.clone();
        match config
            .number
            .or_else(|| Self::numeric_identity_from_name(&name))
        {
            Some(number) => WorkspaceIdentity::Numeric { number, name },
            None => WorkspaceIdentity::Named(name),
        }
    }

    pub fn new(containers: &mut ContainerTree<W>, output: Output, options: Rc<Options>) -> Self {
        Self::new_with_config(containers, output, None, options)
    }

    pub fn new_with_config(
        containers: &mut ContainerTree<W>,
        output: Output,
        mut config: Option<WorkspaceConfig>,
        base_options: Rc<Options>,
    ) -> Self {
        let original_output = config
            .as_ref()
            .and_then(|c| c.open_on_output.clone())
            .map(OutputId)
            .unwrap_or(OutputId::new(&output));

        let identity = Self::identity_from_config(config.as_ref());
        let layout_config = config.as_mut().and_then(|c| c.layout.take().map(|x| x.0));

        let scale = output.current_scale();
        let options = Rc::new(
            Options::clone(&base_options)
                .with_merged_layout(layout_config.as_ref())
                .adjusted_for_scale(scale.fractional_scale()),
        );

        let view_size = output_size(&output);
        let working_area =
            compute_working_area(&output, options.layout.struts, scale.fractional_scale());

        let workspace = containers.add_workspace(
            view_size,
            working_area,
            scale.fractional_scale(),
            options.clone(),
        );

        let shadow_config =
            compute_workspace_shadow_config(options.overview.workspace_shadow, view_size);

        Self {
            workspace,
            runtime: WorkspaceRuntime::new(workspace),
            original_output,
            scale,
            transform: output.current_transform(),
            shadow: Shadow::new(shadow_config),
            background_buffer: SolidColorBuffer::new(view_size, options.layout.background_color),
            output: Some(output),
            base_options,
            identity,
            lifetime: WorkspaceLifetime::Persistent,
            layout_config,
            id: WorkspaceId::next(),
        }
    }

    /// The scratchpad: a workspace that is never on an output.
    ///
    /// sway's `__i3_scratch` is an ordinary workspace on a hidden output, which is what makes
    /// a hidden window an ordinary window — arranged, configured, and in step with its client
    /// rather than waiting to be told what it is on the way back out.
    pub fn new_scratchpad(containers: &mut ContainerTree<W>, options: Rc<Options>) -> Self {
        // The scratchpad is the tree's first root, created like any other workspace: sway's
        // `__i3_scratch` is an ordinary workspace on a hidden output, not a special store.
        let mut workspace = Self::new_with_config_no_outputs(containers, None, options);
        workspace.identity = WorkspaceIdentity::Anonymous;
        workspace
    }

    pub fn new_with_config_no_outputs(
        containers: &mut ContainerTree<W>,
        config: Option<WorkspaceConfig>,
        base_options: Rc<Options>,
    ) -> Self {
        Self::new_with_config_no_outputs_inner(containers, config, base_options)
    }

    fn new_with_config_no_outputs_inner(
        containers: &mut ContainerTree<W>,
        mut config: Option<WorkspaceConfig>,
        base_options: Rc<Options>,
    ) -> Self {
        let original_output = OutputId(
            config
                .as_ref()
                .and_then(|c| c.open_on_output.clone())
                .unwrap_or_default(),
        );

        let identity = Self::identity_from_config(config.as_ref());
        let layout_config = config.as_mut().and_then(|c| c.layout.take().map(|x| x.0));

        let scale = smithay::output::Scale::Integer(1);
        let options = Rc::new(
            Options::clone(&base_options)
                .with_merged_layout(layout_config.as_ref())
                .adjusted_for_scale(scale.fractional_scale()),
        );

        let view_size = Size::from((1280., 720.));
        let working_area = inset_by_struts(
            Rectangle::from_size(view_size),
            options.layout.struts,
            scale.fractional_scale(),
        );

        let workspace = containers.add_workspace(
            view_size,
            working_area,
            scale.fractional_scale(),
            options.clone(),
        );

        let shadow_config =
            compute_workspace_shadow_config(options.overview.workspace_shadow, view_size);

        Self {
            workspace,
            runtime: WorkspaceRuntime::new(workspace),
            output: None,
            scale,
            transform: Transform::Normal,
            original_output,
            shadow: Shadow::new(shadow_config),
            background_buffer: SolidColorBuffer::new(view_size, options.layout.background_color),
            base_options,
            identity,
            lifetime: WorkspaceLifetime::Persistent,
            layout_config,
            id: WorkspaceId::next(),
        }
    }

    pub fn new_no_outputs(containers: &mut ContainerTree<W>, options: Rc<Options>) -> Self {
        Self::new_with_config_no_outputs(containers, None, options)
    }

    /// The size a window gets when the scratchpad floats it.
    ///
    /// `container_floating_set_default_size`: half the workspace's width and three quarters of
    /// its height, clamped by `floating_calculate_constraints`. It is a fraction of the
    /// workspace, not the size the window asked for and not the size it had while tiled —
    /// `floating enable` is the one that keeps a window's own idea of its size, and the
    /// scratchpad is not that.
    ///
    /// The fraction is of the workspace box, which is the usable area; the constraints are of
    /// the whole output layout, which is not. The two are different boxes in sway and are here
    /// too. And it is the *content* size: sway assigns `content_width`/`content_height` and
    /// lets `container_set_geometry_from_content` add the border and title bar.
    ///
    /// sway/tree/container.c:959-980,842-878
    fn scratchpad_default_size(
        &self,
        containers: &ContainerTree<W>,
        tile: &Tile<W>,
    ) -> Size<i32, Logical> {
        let workspace_box = containers.working_area(self.workspace).size;
        let mut size = Size::from((
            (workspace_box.w * 0.5).floor() as i32,
            (workspace_box.h * 0.75).floor() as i32,
        ));

        // `floating_calculate_constraints` on its automatic settings: a floor of 75 by 50, and
        // as the ceiling the box of the whole output layout. The floor is applied outside the
        // ceiling, as sway applies it, so it wins on an output too small to hold it.
        let view_size = containers.view_size(self.workspace);
        size.w = size.w.min(view_size.w.floor() as i32).max(75);
        size.h = size.h.min(view_size.h.floor() as i32).max(50);

        let min_size = tile.window().min_size();
        let max_size = tile.window().max_size();
        size.w = ensure_min_max_size(size.w, min_size.w, max_size.w);
        size.h = ensure_min_max_size(size.h, min_size.h, max_size.h);
        size
    }

    /// This workspace's root node in the layout's tree.
    pub fn workspace_key(&self) -> NodeKey {
        self.workspace
    }

    pub fn id(&self) -> WorkspaceId {
        self.id
    }

    pub fn name(&self) -> Option<&String> {
        match &self.identity {
            WorkspaceIdentity::Anonymous => None,
            WorkspaceIdentity::Numeric { name, .. } | WorkspaceIdentity::Named(name) => Some(name),
        }
    }

    pub(super) fn numeric_number(&self) -> Option<u32> {
        match &self.identity {
            WorkspaceIdentity::Numeric { number, .. } => Some(*number),
            WorkspaceIdentity::Anonymous | WorkspaceIdentity::Named(_) => None,
        }
    }

    pub(super) fn has_persistent_identity(&self) -> bool {
        self.name().is_some() && self.lifetime == WorkspaceLifetime::Persistent
    }

    pub(super) fn make_identity_persistent(&mut self) {
        if self.name().is_some() {
            self.lifetime = WorkspaceLifetime::Persistent;
        }
    }

    pub(super) fn is_empty_transient_numeric(
        &self,
        containers: &ContainerTree<W>,
        number: u32,
    ) -> bool {
        self.numeric_number() == Some(number)
            && self.lifetime == WorkspaceLifetime::Transient
            && !containers.workspace_has_windows(self.workspace)
    }

    pub(super) fn identity(&self) -> &WorkspaceIdentity {
        &self.identity
    }

    pub(super) fn set_numeric_identity(&mut self, number: u32, lifetime: WorkspaceLifetime) {
        self.identity = WorkspaceIdentity::Numeric {
            number,
            name: number.to_string(),
        };
        self.lifetime = lifetime;
    }

    pub(super) fn set_name(&mut self, name: String, lifetime: WorkspaceLifetime) {
        self.identity = WorkspaceIdentity::Named(name);
        self.lifetime = lifetime;
    }

    pub(super) fn unname(&mut self) {
        self.identity = WorkspaceIdentity::Anonymous;
        self.lifetime = WorkspaceLifetime::Transient;
    }

    pub fn has_windows_or_persistent_identity(&self, containers: &ContainerTree<W>) -> bool {
        containers.workspace_has_windows(self.workspace) || self.has_persistent_identity()
    }

    pub(super) fn should_remove_when_empty(
        &self,
        containers: &ContainerTree<W>,
        is_active: bool,
        is_internal_placeholder: bool,
    ) -> bool {
        if is_active || is_internal_placeholder || containers.workspace_has_windows(self.workspace)
        {
            return false;
        }

        self.name().is_none() || self.lifetime == WorkspaceLifetime::Transient
    }

    pub fn scale(&self) -> smithay::output::Scale {
        self.scale
    }

    pub fn advance_animations(&mut self, containers: &mut ContainerTree<W>) {
        containers.advance_animations(self.workspace);
        self.runtime.tiled.closing_windows.retain_mut(|closing| {
            closing.advance_animations();
            closing.are_animations_ongoing()
        });
        self.runtime.floating.advance_closing_animations();
    }

    pub fn are_animations_ongoing(&self, containers: &ContainerTree<W>) -> bool {
        containers.are_animations_ongoing(self.workspace)
            || !self.runtime.tiled.closing_windows.is_empty()
            || self.runtime.floating.has_closing_windows()
    }

    pub fn are_transitions_ongoing(&self, containers: &ContainerTree<W>) -> bool {
        containers.are_transitions_ongoing(self.workspace)
            || !self.runtime.tiled.closing_windows.is_empty()
            || self.runtime.floating.has_closing_windows()
    }

    pub fn update_render_elements(
        &mut self,
        containers: &mut ContainerTree<W>,
        is_active: bool,
        layer: RenderLayer,
    ) {
        self.runtime.is_active = is_active;
        let tiling_has_focus = self.runtime.layer_has_focus(LayoutLayer::Tiled);
        let floating_has_focus = self.runtime.layer_has_focus(LayoutLayer::Floating);
        containers.update_render_elements(
            self.workspace,
            tiling_has_focus,
            &self.runtime.tiled.interactive_resize,
        );

        let view_rect = Rectangle::from_size(containers.view_size(self.workspace));
        self.runtime.floating.update_render_elements(
            containers,
            floating_has_focus,
            view_rect,
            layer,
        );

        if layer.is_normal() {
            self.shadow.update_render_elements(
                containers.view_size(self.workspace),
                true,
                CornerRadius::default(),
                self.scale.fractional_scale(),
                1.,
            );
        }
    }

    pub fn update_config(&mut self, containers: &mut ContainerTree<W>, base_options: Rc<Options>) {
        let scale = self.scale.fractional_scale();
        let options = Rc::new(
            Options::clone(&base_options)
                .with_merged_layout(self.layout_config.as_ref())
                .adjusted_for_scale(scale),
        );

        let view_size = containers.view_size(self.workspace);

        // Struts are the one layout option that changes the box everything is laid out in, so a
        // reload has to re-derive it rather than pass the box it was given on the last one.
        let working_area =
            if options.layout.struts != containers.options(self.workspace).layout.struts {
                match self.output.as_ref() {
                    Some(output) => compute_working_area(output, options.layout.struts, scale),
                    None => inset_by_struts(
                        Rectangle::from_size(view_size),
                        options.layout.struts,
                        scale,
                    ),
                }
            } else {
                containers.working_area(self.workspace)
            };

        containers.update_config(
            self.workspace,
            view_size,
            working_area,
            scale,
            options.clone(),
        );

        let shadow_config =
            compute_workspace_shadow_config(options.overview.workspace_shadow, view_size);
        self.shadow.update_config(shadow_config);

        self.background_buffer
            .set_color(options.layout.background_color);

        self.base_options = base_options;
    }

    pub fn update_layout_config(
        &mut self,
        containers: &mut ContainerTree<W>,
        layout_config: Option<tiri_config::LayoutPart>,
    ) {
        if self.layout_config == layout_config {
            return;
        }

        self.layout_config = layout_config;
        self.update_config(containers, self.base_options.clone());
    }

    pub fn update_shaders(&mut self, containers: &mut ContainerTree<W>) {
        containers.update_shaders(self.workspace);
        self.shadow.update_shaders();
    }

    fn is_floating_target(&self, containers: &ContainerTree<W>, window: Option<&W::Id>) -> bool {
        window.map_or(self.runtime.layer.floating_has_focus(), |id| {
            containers.window_is_floating_in(self.workspace, id)
        })
    }

    fn command_target(&self, containers: &ContainerTree<W>) -> WorkspaceCommandTarget {
        containers.workspace_command_target(self.workspace)
    }

    /// The one node commands address, for operations that do not branch on its layer.
    fn command_target_key(&self, containers: &ContainerTree<W>) -> NodeKey {
        self.command_target(containers).key(self.workspace)
    }

    pub(super) fn apply_swap_effects(
        &mut self,
        containers: &ContainerTree<W>,
        effects: SwapEffects,
    ) {
        self.runtime.apply_swap_effects(containers, effects);
        self.sync_active_layer_to_command_target(containers);
    }

    /// Keep the render/input layer aligned after a tree mutation changes selection. Selecting
    /// the workspace itself deliberately preserves the last active side; every other node names
    /// its side through ancestry.
    fn sync_active_layer_to_command_target(&mut self, containers: &ContainerTree<W>) {
        match self.command_target(containers) {
            WorkspaceCommandTarget::Workspace => {}
            WorkspaceCommandTarget::Tiled(_) => self.runtime.layer = WorkspaceLayerState::Tiled,
            WorkspaceCommandTarget::Floating(_) => {
                self.runtime.layer = WorkspaceLayerState::Floating;
            }
        }
    }

    /// Route a focus command to the active layer.
    ///
    /// Centralizes the tiling-side `activate_tiling_content()` follow-up so it
    /// can never be forgotten by an individual focus method — the historical source of stale
    /// workspace-context bugs.
    fn dispatch_focus(
        &mut self,
        containers: &mut ContainerTree<W>,
        floating: impl FnOnce(&mut FloatingRuntime<W>, &mut ContainerTree<W>) -> bool,
        tiled: impl FnOnce(&mut ContainerTree<W>) -> bool,
    ) -> bool {
        self.dispatch_focus_from_workspace(containers, |_| false, floating, tiled)
    }

    /// The same, for the commands sway also answers from the workspace node itself.
    ///
    /// Three arms because sway's `cmd_focus` has three cases: the workspace, a node in
    /// `ws->floating`, and everything else. Routing them in one place is not tidiness — it
    /// is what makes the epilogue unforgettable. Landing in the tiled tree has to leave two
    /// things true, that the active layer names the tiled side and that no workspace
    /// selection outlives the descent, and a method that settled one of them and not the
    /// other is how `focus child` from a floating window ended up focusing nothing.
    fn dispatch_focus_from_workspace(
        &mut self,
        containers: &mut ContainerTree<W>,
        workspace: impl FnOnce(&mut ContainerTree<W>) -> bool,
        floating: impl FnOnce(&mut FloatingRuntime<W>, &mut ContainerTree<W>) -> bool,
        tiled: impl FnOnce(&mut ContainerTree<W>) -> bool,
    ) -> bool {
        // The workspace's own arm descends into `ws->tiling`, the same side `tiled` acts on,
        // so both take the tiled epilogue. Only the floating arm leaves the layer alone.
        let (moved, landed_in_tiling) = match self.command_target(containers) {
            WorkspaceCommandTarget::Workspace => (workspace(containers), true),
            WorkspaceCommandTarget::Floating(_) => {
                (floating(&mut self.runtime.floating, containers), false)
            }
            WorkspaceCommandTarget::Tiled(_) => (tiled(containers), true),
        };
        if landed_in_tiling && moved {
            self.sync_active_layer_to_command_target(containers);
            self.activate_tiling_content(containers);
        }
        moved
    }

    /// Route a directional move to the active layer.
    ///
    /// sway translates a top-level floating container geometrically, but a selected descendant
    /// is first allowed to move structurally inside that floating branch. The floating layer
    /// always reports the command as handled, even when the structural move reaches its root and
    /// becomes a no-op; Tiri's combined move-or-workspace actions must not fall through then.
    fn dispatch_move_directional(
        &mut self,
        containers: &mut ContainerTree<W>,
        direction: Direction,
        floating: impl FnOnce(&mut FloatingRuntime<W>, &mut ContainerTree<W>),
        tiled: impl FnOnce(&mut ContainerTree<W>) -> bool,
    ) -> bool {
        match self.command_target(containers) {
            WorkspaceCommandTarget::Workspace => false,
            WorkspaceCommandTarget::Floating(target) => {
                if containers.is_floating_root(target) {
                    floating(&mut self.runtime.floating, containers);
                } else {
                    let branch = containers.branch_root(target);
                    let _ = containers.move_in_branch(branch, direction);
                }
                true
            }
            WorkspaceCommandTarget::Tiled(_) => tiled(containers),
        }
    }

    /// Route by the active layer only, ignoring workspace focus elevation: these commands
    /// resolve their own target inside the layer (i3 falls back to the focused leaf).
    fn dispatch_active_layer<R>(
        &mut self,
        containers: &mut ContainerTree<W>,
        floating: impl FnOnce(&mut FloatingRuntime<W>, &mut ContainerTree<W>) -> R,
        tiled: impl FnOnce(&mut ContainerTree<W>) -> R,
    ) -> R {
        if self.runtime.layer.floating_has_focus() {
            floating(&mut self.runtime.floating, containers)
        } else {
            tiled(containers)
        }
    }

    /// Route by the layer `window` lives in, defaulting to the active layer for `None`.
    fn dispatch_for_window<R>(
        &mut self,
        containers: &mut ContainerTree<W>,
        window: Option<&W::Id>,
        floating: impl FnOnce(&mut FloatingRuntime<W>, &mut ContainerTree<W>) -> R,
        tiled: impl FnOnce(&mut ContainerTree<W>) -> R,
    ) -> R {
        if self.is_floating_target(containers, window) {
            floating(&mut self.runtime.floating, containers)
        } else {
            tiled(containers)
        }
    }

    /// Route one of Tiri's root-ordering commands. These commands address the tiled root list;
    /// directional `move container` commands use `dispatch_move_directional` on both layers.
    fn dispatch_tiling_root_reorder<R: Default>(
        &mut self,
        containers: &mut ContainerTree<W>,
        tiled: impl FnOnce(&mut ContainerTree<W>) -> R,
    ) -> R {
        match self.command_target(containers) {
            WorkspaceCommandTarget::Tiled(_) => tiled(containers),
            WorkspaceCommandTarget::Workspace | WorkspaceCommandTarget::Floating(_) => R::default(),
        }
    }

    pub fn focus_mode_toggle_targets_floating(&self, containers: &ContainerTree<W>) -> bool {
        !matches!(
            self.command_target(containers),
            WorkspaceCommandTarget::Floating(_)
        )
    }

    pub fn current_output(&self) -> Option<&Output> {
        self.output.as_ref()
    }

    pub fn active_window<'a>(&'a self, containers: &'a ContainerTree<W>) -> Option<&'a W> {
        if self.runtime.layer.floating_has_focus() {
            containers.active_floating_window(self.workspace)
        } else {
            containers.active_tiled_window(self.workspace)
        }
    }

    pub fn active_window_mut<'a>(
        &'a mut self,
        containers: &'a mut ContainerTree<W>,
    ) -> Option<&'a mut W> {
        if self.runtime.layer.floating_has_focus() {
            containers.active_floating_window_mut(self.workspace)
        } else {
            containers.active_tiled_window_mut(self.workspace)
        }
    }

    pub fn active_selection_is_container(&self, containers: &ContainerTree<W>) -> bool {
        match self.command_target(containers) {
            WorkspaceCommandTarget::Workspace => false,
            WorkspaceCommandTarget::Tiled(key) | WorkspaceCommandTarget::Floating(key) => {
                !containers.is_leaf(key)
            }
        }
    }

    #[cfg(test)]
    pub(super) fn marks_for_window(
        &self,
        containers: &ContainerTree<W>,
        id: &W::Id,
    ) -> Vec<String> {
        containers
            .window_key(id)
            .and_then(|key| containers.node_marks(key))
            .unwrap_or_default()
            .to_vec()
    }

    pub fn close_window_ids_for_active_selection(
        &self,
        containers: &ContainerTree<W>,
    ) -> Vec<W::Id> {
        let ids = match self.command_target(containers) {
            WorkspaceCommandTarget::Workspace => {
                return containers
                    .windows_in_workspace(self.workspace)
                    .map(|window| window.id().clone())
                    .collect();
            }
            WorkspaceCommandTarget::Floating(_) => self
                .runtime
                .floating
                .close_window_ids_for_active_selection(containers),
            WorkspaceCommandTarget::Tiled(_) => {
                containers.close_window_ids_for_active_selection(self.workspace)
            }
        };
        if !ids.is_empty() {
            return ids;
        }

        self.active_window(containers)
            .map(|window| vec![window.id().clone()])
            .unwrap_or_default()
    }

    pub fn set_output(&mut self, containers: &mut ContainerTree<W>, output: Option<Output>) {
        if self.output == output {
            return;
        }

        if let Some(output) = self.output.take() {
            for win in containers.windows_in_workspace(self.workspace) {
                win.output_leave(&output);
            }
        }

        self.output = output;

        if let Some(output) = &self.output {
            // Normalize original output: possibly replace connector with make/model/serial.
            if self.original_output.matches(output) {
                self.original_output = OutputId::new(output);
            }

            self.update_output_size(containers);

            for win in containers.windows_in_workspace(self.workspace) {
                self.enter_output_for_window(win);
            }
        }
    }

    fn enter_output_for_window(&self, window: &W) {
        if let Some(output) = &self.output {
            window.set_preferred_scale_transform(self.scale, self.transform);
            window.output_enter(output);
        }
    }

    pub fn update_output_size(&mut self, containers: &mut ContainerTree<W>) {
        let output = self.output.as_ref().unwrap();
        let scale = output.current_scale();
        let transform = output.current_transform();
        let view_size = output_size(output);
        let working_area = compute_working_area(
            output,
            containers.options(self.workspace).layout.struts,
            scale.fractional_scale(),
        );
        self.set_view_size(containers, scale, transform, view_size, working_area);
    }

    fn set_view_size(
        &mut self,
        containers: &mut ContainerTree<W>,
        scale: smithay::output::Scale,
        transform: Transform,
        size: Size<f64, Logical>,
        working_area: Rectangle<f64, Logical>,
    ) {
        let scale_transform_changed = self.transform != transform
            || self.scale.integer_scale() != scale.integer_scale()
            || self.scale.fractional_scale() != scale.fractional_scale();
        if !scale_transform_changed
            && containers.view_size(self.workspace) == size
            && containers.working_area(self.workspace) == working_area
        {
            return;
        }

        let fractional_scale_changed = self.scale.fractional_scale() != scale.fractional_scale();

        self.scale = scale;
        self.transform = transform;

        // The node owns the box, so this is the write that records the new one, and it comes
        // before the branch because both arms re-derive from what the node holds: the scale
        // arm through `update_config`, which reads the view size back out to re-inset it by
        // the struts.
        let options = containers.options(self.workspace).clone();
        containers.update_config(
            self.workspace,
            size,
            working_area,
            scale.fractional_scale(),
            options.clone(),
        );

        if fractional_scale_changed {
            // Options need to be recomputed for the new scale.
            self.update_config(containers, self.base_options.clone());
        } else {
            let shadow_config = compute_workspace_shadow_config(
                containers.options(self.workspace).overview.workspace_shadow,
                size,
            );
            self.shadow.update_config(shadow_config);
        }

        self.background_buffer.resize(size);

        if scale_transform_changed {
            for window in containers.windows_in_workspace(self.workspace) {
                window.set_preferred_scale_transform(self.scale, self.transform);
            }
        }
    }

    pub fn make_tile(&self, containers: &ContainerTree<W>, window: W) -> NewView<W> {
        NewView::new(Tile::new(
            window,
            containers.view_size(self.workspace),
            self.scale.fractional_scale(),
            containers.clock().clone(),
            containers.options(self.workspace).clone(),
        ))
    }

    #[allow(clippy::too_many_arguments)]
    pub(super) fn add_tile(
        &mut self,
        containers: &mut ContainerTree<W>,
        mut tile: IncomingView<W>,
        target: WorkspaceAddWindowTarget<W>,
        activate: ActivateWindow,
        is_floating: bool,
    ) {
        self.enter_output_for_window(
            containers
                .incoming_tile(&tile)
                .expect("a workspace insertion must own a valid incoming view")
                .window(),
        );
        let floating_has_focus = self.runtime.layer.floating_has_focus();
        let workspace_command_context = matches!(
            self.command_target(containers),
            WorkspaceCommandTarget::Workspace
        );
        let map_target = containers.view_map_target(self.workspace);
        let map_targets_floating =
            map_target.is_some_and(|target| containers.is_in_floating_branch(target));
        // A group root occupies the `ws->floating` slot and is not an insertion parent for a
        // newly mapped normal view. Any node below that boundary is: a leaf, or an inner
        // container created by splitting the root. This is derived from Sway's view-map
        // target, not merely from the currently selected node: selecting the workspace keeps
        // the previous node immediately behind it in the seat order.
        let map_targets_inside_floating_root = map_target
            .is_some_and(|target| map_targets_floating && !containers.is_floating_root(target));
        match target {
            WorkspaceAddWindowTarget::Auto => {
                let has_floating_reinsert_hint = containers
                    .incoming_tile(&tile)
                    .expect("a workspace insertion must own a valid incoming view")
                    .floating_reinsert_hint
                    .is_some();
                // A focused node below the `ws->floating` list boundary is an insertion
                // parent for a newly mapped window. The root entry itself is not. This is a
                // structural distinction: a workspace wrapper can acquire an inner split,
                // and that split accepts a sibling just like one built around a single view.
                // A tile returning from an interactive move carries its original floating
                // parent explicitly and must not be absorbed by whichever group is active now.
                let grouped_floating = !has_floating_reinsert_hint
                    && floating_has_focus
                    && map_targets_inside_floating_root;
                let wants_floating = is_floating || grouped_floating;

                let activate = if !wants_floating
                    && containers.has_fullscreen_window(self.workspace)
                {
                    // Model rule: while a tiling window is fullscreen, newly opened tiling windows
                    // should not steal focus.
                    false
                } else {
                    // Don't steal focus from an active fullscreen window.
                    activate.map_smart(|| !containers.is_active_pending_fullscreen(self.workspace))
                };

                if wants_floating {
                    if has_floating_reinsert_hint {
                        self.runtime
                            .floating
                            .add_tile_with_restore_hint(containers, tile, activate);
                    } else if grouped_floating {
                        self.runtime
                            .floating
                            .add_tile_to_active_container(containers, tile, activate);
                    } else {
                        self.runtime.floating.add_tile(containers, tile, activate);
                    }

                    if activate || containers.tiled_is_empty(self.workspace) {
                        self.activate_floating_for_new_content(containers);
                    }
                } else {
                    let tiling_was_empty = containers.tiled_is_empty(self.workspace);
                    containers.add_tile(self.workspace, None, tile, activate);

                    if activate
                        || (floating_has_focus
                            && tiling_was_empty
                            && !wants_floating
                            && !workspace_command_context)
                    {
                        self.activate_tiling_for_new_content(containers);
                    }
                }
            }
            WorkspaceAddWindowTarget::AtRootIndex(root_index) => {
                let activate = activate.map_smart(|| false);
                containers.add_tile(self.workspace, Some(root_index), tile, activate);

                if activate {
                    self.activate_tiling_for_new_content(containers);
                }
            }
            WorkspaceAddWindowTarget::NextTo(next_to) => {
                let floating_has_window = containers.window_is_floating_in(self.workspace, next_to);
                let grouped_floating_target = floating_has_window
                    && self
                        .runtime
                        .floating
                        .container_allows_splits(containers, next_to);
                let wants_floating = is_floating || grouped_floating_target;

                let activate = activate.map_smart(|| {
                    self.active_window(containers)
                        .is_some_and(|win| win.id() == next_to)
                });

                if wants_floating {
                    if grouped_floating_target {
                        self.runtime
                            .floating
                            .add_tile_to_container_of(containers, next_to, tile, activate);
                    } else if floating_has_window {
                        self.runtime
                            .floating
                            .add_tile_above(containers, next_to, tile, activate);
                    } else {
                        self.center_new_incoming_floating_tile_on(containers, &mut tile, next_to);
                        self.runtime.floating.add_tile(containers, tile, activate);
                    }

                    if activate || containers.tiled_is_empty(self.workspace) {
                        self.activate_floating_for_new_content(containers);
                    }
                } else if floating_has_window {
                    containers.add_tile(self.workspace, None, tile, activate);

                    if activate {
                        self.activate_tiling_for_new_content(containers);
                    }
                } else {
                    if containers
                        .tiled_tiles(self.workspace)
                        .any(|tile| tile.window().id() == next_to)
                    {
                        containers.add_tile_right_of(self.workspace, next_to, tile, activate);
                    } else {
                        error!("next_to target disappeared while placing a new tiled window");
                        containers.add_tile(self.workspace, None, tile, activate);
                    }

                    if activate {
                        // The only add path that also records the restore chain. Nothing has
                        // measured whether the other two are the ones that are wrong: sway has
                        // no such stack, so only a recording of what it focuses after an open
                        // can settle it.
                        self.runtime.layer = WorkspaceLayerState::Tiled;
                        self.activate_tiling_content(containers);
                    }
                }
            }
        }

        // The workspace's fullscreen pointer is one for the whole workspace, so an arriving
        // window that still requests fullscreen has to be reconciled against whatever is
        // already holding it. The tiled inserts do this inside the tree; the floating ones
        // never did, which let two floating clients sit pending fullscreen side by side. sway
        // resolves the same collision in `container_handle_fullscreen_reparent`.
        containers.sync_fullscreen_window(self.workspace);
    }

    fn center_new_incoming_floating_tile_on(
        &self,
        containers: &mut ContainerTree<W>,
        tile: &mut IncomingView<W>,
        next_to: &W::Id,
    ) {
        let Some((next_to_size, render_pos)) = containers
            .tiled_tiles_with_render_positions(self.workspace)
            .find_map(|(tile, render_pos, _)| {
                (tile.window().id() == next_to).then_some((tile.tile_size(), render_pos))
            })
        else {
            error!("next_to target disappeared while placing a new floating window");
            return;
        };

        let working_area = containers.working_area(self.workspace);
        let tile = containers
            .incoming_tile_mut(tile)
            .expect("a workspace insertion must own a valid incoming view");
        let tile_size = tile.tile_size();
        let pos = render_pos + (next_to_size.to_point() - tile_size.to_point()).downscale(2.);
        let pos = self
            .runtime
            .floating
            .clamp_within_working_area(working_area, pos, tile_size);
        tile.floating_pos = Some(
            self.runtime
                .floating
                .logical_to_size_frac(working_area, pos),
        );
    }

    pub(super) fn tiling_insert_parent_info(
        &self,
        containers: &ContainerTree<W>,
        window: &W::Id,
    ) -> Option<InsertParentInfo> {
        containers.insert_parent_info_for_window(window)
    }

    fn inactive_tiling_reference(
        &self,
        containers: &ContainerTree<W>,
    ) -> Option<InactiveTilingReference> {
        containers.inactive_tiling_reference(self.workspace)
    }

    fn activate_tiling_content(&mut self, containers: &mut ContainerTree<W>) {
        containers.clear_workspace_selection(self.workspace);
    }

    fn focus_tiling_key(
        &mut self,
        containers: &mut ContainerTree<W>,
        key: super::container::NodeKey,
    ) -> bool {
        let focused = containers.focus_inactive_tiling_key(key);
        if focused {
            self.runtime.layer = WorkspaceLayerState::Tiled;
            self.activate_tiling_content(containers);
        }
        focused
    }

    fn window_has_fullscreen_focus_scope(&self, containers: &ContainerTree<W>, window: &W) -> bool {
        containers.is_fullscreen(self.workspace, window)
            || window.pending_sizing_mode().is_fullscreen()
            || window.is_pending_windowed_fullscreen()
    }

    fn tiling_key_focusable(
        &self,
        containers: &ContainerTree<W>,
        key: super::container::NodeKey,
    ) -> bool {
        let any_fullscreen = containers
            .windows_in_workspace(self.workspace)
            .any(|window| self.window_has_fullscreen_focus_scope(containers, window));
        if !any_fullscreen {
            return true;
        }

        containers
            .window_for_inactive_tiling_key(key)
            .is_some_and(|window| self.window_has_fullscreen_focus_scope(containers, window))
    }

    pub(super) fn focus_floating_window(
        &mut self,
        containers: &mut ContainerTree<W>,
        id: &W::Id,
        raise: bool,
    ) -> bool {
        let focused = if raise {
            self.runtime.floating.activate_window(containers, id)
        } else {
            self.runtime
                .floating
                .activate_window_without_raising(containers, id)
        };
        if focused {
            self.runtime.layer = WorkspaceLayerState::Floating;
        }
        focused
    }

    pub(super) fn restore_inactive_floating(&mut self, containers: &mut ContainerTree<W>) -> bool {
        let Some(id) = containers.inactive_floating_window_id(self.workspace) else {
            return false;
        };
        self.focus_floating_window(containers, &id, false)
    }

    pub(super) fn restore_inactive_tiling(
        &mut self,
        containers: &mut ContainerTree<W>,
    ) -> Option<bool> {
        let key = containers.inactive_tiling_key(self.workspace)?;
        if !self.tiling_key_focusable(containers, key) {
            return Some(false);
        }
        Some(self.focus_tiling_key(containers, key))
    }

    pub(super) fn focus_targets_window(&self, containers: &ContainerTree<W>, id: &W::Id) -> bool {
        if self.focus_is_elevated(containers) {
            return false;
        }
        self.active_window(containers)
            .is_some_and(|window| window.id() == id)
    }

    /// Swap a dragged tile with the leaf at `target`, sending the displaced tile back to
    /// `origin` (where the dragged tile came from).
    ///
    /// Hands the tile back as `Err` when `target` is no longer a tiling leaf, leaving the
    /// tree untouched so the caller can fall back to a plain insert.
    // The Err variant carries the tile back to the caller; boxing it would only add an
    // allocation to the failure path.
    pub(super) fn tiling_swap_tile(
        &mut self,
        containers: &mut ContainerTree<W>,
        target: NodeKey,
        tile: IncomingView<W>,
        origin: &InsertParentInfo,
    ) -> Result<(), IncomingView<W>> {
        if !containers.is_tiling_leaf(target) {
            return Err(tile);
        }
        // A refusal hands the dragged view back untouched, so the caller still has a window to
        // fall back with. Nothing has entered the tree.
        let displaced = containers.replace_tiling_tile(target, tile)?;
        containers
            .insert_tile_with_parent_info(self.workspace, origin, displaced, false)
            .expect("the origin workspace must accept the displaced view");
        Ok(())
    }

    pub(super) fn add_tile_split(
        &mut self,
        containers: &mut ContainerTree<W>,
        target: NodeKey,
        direction: Direction,
        mut tile: IncomingView<W>,
        activate: bool,
    ) -> Result<(), IncomingView<W>> {
        containers
            .incoming_tile_mut(&mut tile)
            .expect("a workspace insertion must own a valid incoming view")
            .set_scratchpad(false);
        self.enter_output_for_window(
            containers
                .incoming_tile(&tile)
                .expect("a workspace insertion must own a valid incoming view")
                .window(),
        );

        containers.insert_tile_split(target, direction, tile, activate)?;

        if activate {
            self.runtime.layer = WorkspaceLayerState::Tiled;
            self.activate_tiling_content(containers);
        }

        Ok(())
    }

    pub(super) fn add_tile_split_root(
        &mut self,
        containers: &mut ContainerTree<W>,
        direction: Direction,
        mut tile: IncomingView<W>,
        activate: bool,
    ) -> Result<(), IncomingView<W>> {
        containers
            .incoming_tile_mut(&mut tile)
            .expect("a workspace insertion must own a valid incoming view")
            .set_scratchpad(false);
        self.enter_output_for_window(
            containers
                .incoming_tile(&tile)
                .expect("a workspace insertion must own a valid incoming view")
                .window(),
        );

        containers.insert_tile_split_root(self.workspace, direction, tile, activate)?;

        if activate {
            self.runtime.layer = WorkspaceLayerState::Tiled;
            self.activate_tiling_content(containers);
        }

        Ok(())
    }

    pub fn add_root_tiling_subtree(
        &mut self,
        containers: &mut ContainerTree<W>,
        subtree: RootTilingSubtree,
        activate: bool,
    ) {
        for tile in subtree.tiles(containers) {
            self.enter_output_for_window(tile.window());
        }

        containers.add_root_tiling_subtree(self.workspace, None, subtree, activate);

        if activate {
            self.runtime.layer = WorkspaceLayerState::Tiled;
            self.activate_tiling_content(containers);
        }
    }

    fn update_focus_floating_tiling_after_removing(
        &mut self,
        containers: &mut ContainerTree<W>,
        removed_from_floating: bool,
    ) {
        // An elevation can only belong to the active layer (the inactive layer's elevation is
        // already dropped by construction), so clear it when that active layer empties out.
        if containers.tiled_is_empty(self.workspace) {
            containers.clear_selection_context();
        }

        if removed_from_floating {
            if containers.floating_root_count(self.workspace) == 0 {
                self.runtime.layer = WorkspaceLayerState::Tiled;
                self.activate_tiling_content(containers);
            }
        } else {
            // Tiling should remain focused if both are empty.
            if containers.tiled_is_empty(self.workspace)
                && containers.floating_root_count(self.workspace) != 0
            {
                self.runtime.layer = WorkspaceLayerState::Floating;
            }
        }
    }

    pub(super) fn take_tile_for_transfer(
        &mut self,
        containers: &mut ContainerTree<W>,
        id: &W::Id,
        transaction: Transaction,
    ) -> ViewTransfer {
        let key = containers
            .window_key(id)
            .expect("the window to transfer must exist in the tree");
        assert_eq!(containers.workspace_of(key), Some(self.workspace));
        let from_floating = containers.is_in_floating_branch(key);
        let transferred = if from_floating {
            self.runtime.floating.take_tile_for_transfer(containers, id)
        } else {
            containers.take_tile_for_transfer(self.workspace, id, transaction)
        };

        if let Some(output) = &self.output {
            transferred.tile(containers).window().output_leave(output);
        }

        self.update_focus_floating_tiling_after_removing(containers, from_floating);
        transferred
    }

    pub fn remove_active_root_tiling_subtree(
        &mut self,
        containers: &mut ContainerTree<W>,
    ) -> Option<RootTilingSubtree> {
        let from_floating = self.runtime.layer.floating_has_focus();
        if from_floating {
            return None;
        }

        let subtree = containers.remove_active_root_tiling_subtree(self.workspace)?;

        if let Some(output) = &self.output {
            for tile in subtree.tiles(containers) {
                tile.window().output_leave(output);
            }
        }

        self.update_focus_floating_tiling_after_removing(containers, from_floating);

        Some(subtree)
    }

    pub fn remove_active_tiling_subtree(
        &mut self,
        containers: &mut ContainerTree<W>,
    ) -> Option<RootTilingSubtree> {
        let from_floating = self.runtime.layer.floating_has_focus();
        if from_floating {
            return None;
        }

        let subtree = containers.remove_active_tiling_subtree(self.workspace)?;

        if let Some(output) = &self.output {
            for tile in subtree.tiles(containers) {
                tile.window().output_leave(output);
            }
        }

        self.update_focus_floating_tiling_after_removing(containers, from_floating);

        Some(subtree)
    }

    pub fn resolve_default_width(
        &self,
        containers: &ContainerTree<W>,
        default_width: Option<Option<PresetSize>>,
        is_floating: bool,
    ) -> Option<PresetSize> {
        match default_width {
            Some(Some(width)) => Some(width),
            Some(None) => None,
            None if is_floating => None,
            None => {
                containers
                    .options(self.workspace)
                    .layout
                    .default_column_width
            }
        }
    }

    pub fn resolve_default_height(
        &self,
        default_height: Option<Option<PresetSize>>,
        is_floating: bool,
    ) -> Option<PresetSize> {
        match default_height {
            Some(Some(height)) => Some(height),
            Some(None) => None,
            None if is_floating => None,
            // We don't have a global default at the moment.
            None => None,
        }
    }

    pub fn new_window_size(
        &self,
        containers: &ContainerTree<W>,
        width: Option<PresetSize>,
        height: Option<PresetSize>,
        is_floating: bool,
        rules: &ResolvedWindowRules,
        (min_size, max_size): (Size<i32, Logical>, Size<i32, Logical>),
    ) -> Size<i32, Logical> {
        let mut size = if is_floating {
            self.runtime
                .floating
                .new_window_size(containers, width, height, rules)
        } else {
            containers.new_window_size(self.workspace, width, height, rules)
        };

        // If the window has a fixed size, or we're picking some fixed size, apply min and max
        // size. This is to ensure that a fixed-size window rule works on open, while still
        // allowing the window freedom to pick its default size otherwise.
        let (min_size, max_size) = rules.apply_min_max_size(min_size, max_size);
        size.w = ensure_min_max_size_maybe_zero(size.w, min_size.w, max_size.w);
        // For tiling (where height is > 0) only ensure fixed height, since runtime tiling will
        // only honor fixed height currently.
        if min_size.h == max_size.h {
            size.h = ensure_min_max_size(size.h, min_size.h, max_size.h);
        } else if size.h > 0 {
            // Also always honor min height, tiling always does.
            size.h = max(size.h, min_size.h);
        }

        size
    }

    pub fn configure_new_window(
        &self,
        containers: &ContainerTree<W>,
        window: &Window,
        width: Option<PresetSize>,
        height: Option<PresetSize>,
        is_floating: bool,
        rules: &ResolvedWindowRules,
    ) {
        window.with_surfaces(|surface, data| {
            send_scale_transform(surface, data, self.scale, self.transform);
        });

        let toplevel = window.toplevel().expect("no x11 support");
        let (min_size, max_size) = with_states(toplevel.wl_surface(), |state| {
            let mut guard = state.cached_state.get::<SurfaceCachedState>();
            let current = guard.current();
            (current.min_size, current.max_size)
        });
        toplevel.with_pending_state(|state| {
            if state.states.contains(xdg_toplevel::State::Fullscreen) {
                state.size = Some(containers.view_size(self.workspace).to_i32_round());
            } else if !is_floating {
                // Like sway, let an ordinary tiled window choose the geometry it maps with.
                // Mapped stores that geometry as the natural floating size before tiling sends
                // its post-map configure. Sending the tiled size here would erase that
                // distinction for clients that obey their initial configure.
                state.size = None;
            } else {
                let size = self.new_window_size(
                    containers,
                    width,
                    height,
                    is_floating,
                    rules,
                    (min_size, max_size),
                );
                state.size = Some(size);
            }

            if is_floating {
                let border = containers
                    .options(self.workspace)
                    .layout
                    .border
                    .merged_with(&rules.border);
                state.bounds = Some(compute_toplevel_bounds(
                    border,
                    containers.working_area(self.workspace).size,
                ));
            } else {
                state.bounds = Some(containers.new_window_toplevel_bounds(rules));
            }
        });
    }

    pub fn focus_left(&mut self, containers: &mut ContainerTree<W>) -> bool {
        let workspace = self.workspace;
        self.dispatch_focus(
            containers,
            |f, tree| f.focus_left(tree),
            |t| t.focus_left(workspace),
        )
    }

    pub fn focus_left_no_wrap(&mut self, containers: &mut ContainerTree<W>) -> bool {
        let workspace = self.workspace;
        self.dispatch_focus(
            containers,
            |f, tree| f.focus_left_no_wrap(tree),
            |t| t.focus_left_no_wrap(workspace),
        )
    }

    pub fn focus_right(&mut self, containers: &mut ContainerTree<W>) -> bool {
        let workspace = self.workspace;
        self.dispatch_focus(
            containers,
            |f, tree| f.focus_right(tree),
            |t| t.focus_right(workspace),
        )
    }

    pub fn focus_right_no_wrap(&mut self, containers: &mut ContainerTree<W>) -> bool {
        let workspace = self.workspace;
        self.dispatch_focus(
            containers,
            |f, tree| f.focus_right_no_wrap(tree),
            |t| t.focus_right_no_wrap(workspace),
        )
    }

    pub fn focus_root_container_first(&mut self, containers: &mut ContainerTree<W>) {
        let workspace = self.workspace;
        self.dispatch_focus(
            containers,
            |f, tree| {
                f.focus_leftmost(tree);
                true
            },
            |t| {
                t.focus_root_container_first(workspace);
                true
            },
        );
    }

    pub fn focus_root_container_last(&mut self, containers: &mut ContainerTree<W>) {
        let workspace = self.workspace;
        self.dispatch_focus(
            containers,
            |f, tree| {
                f.focus_rightmost(tree);
                true
            },
            |t| {
                t.focus_root_container_last(workspace);
                true
            },
        );
    }

    pub fn focus_root_container_right_or_first(&mut self, containers: &mut ContainerTree<W>) {
        if !self.focus_right(containers) {
            self.focus_root_container_first(containers);
        }
    }

    pub fn focus_root_container_left_or_last(&mut self, containers: &mut ContainerTree<W>) {
        if !self.focus_left(containers) {
            self.focus_root_container_last(containers);
        }
    }

    pub fn focus_root_container(&mut self, containers: &mut ContainerTree<W>, index: usize) {
        if self.runtime.layer.floating_has_focus() {
            self.focus_tiling(containers);
        }
        containers.focus_root_container(self.workspace, index);
        self.activate_tiling_content(containers);
    }

    pub fn focus_leaf_in_root_container(&mut self, containers: &mut ContainerTree<W>, index: u8) {
        if self.runtime.layer.floating_has_focus() {
            return;
        }
        containers.focus_leaf_in_root_container(self.workspace, index);
        self.activate_tiling_content(containers);
    }

    pub fn focus_down(&mut self, containers: &mut ContainerTree<W>) -> bool {
        let workspace = self.workspace;
        self.dispatch_focus(
            containers,
            |f, tree| f.focus_down(tree),
            |t| t.focus_down(workspace),
        )
    }

    pub fn focus_up(&mut self, containers: &mut ContainerTree<W>) -> bool {
        let workspace = self.workspace;
        self.dispatch_focus(
            containers,
            |f, tree| f.focus_up(tree),
            |t| t.focus_up(workspace),
        )
    }

    pub fn focus_down_or_left(&mut self, containers: &mut ContainerTree<W>) {
        let workspace = self.workspace;
        self.dispatch_focus(
            containers,
            |f, tree| {
                f.focus_down(tree);
                true
            },
            |t| {
                t.focus_down_or_left(workspace);
                true
            },
        );
    }

    pub fn focus_down_or_right(&mut self, containers: &mut ContainerTree<W>) {
        let workspace = self.workspace;
        self.dispatch_focus(
            containers,
            |f, tree| {
                f.focus_down(tree);
                true
            },
            |t| {
                t.focus_down_or_right(workspace);
                true
            },
        );
    }

    pub fn focus_up_or_left(&mut self, containers: &mut ContainerTree<W>) {
        let workspace = self.workspace;
        self.dispatch_focus(
            containers,
            |f, tree| {
                f.focus_up(tree);
                true
            },
            |t| {
                t.focus_up_or_left(workspace);
                true
            },
        );
    }

    pub fn focus_up_or_right(&mut self, containers: &mut ContainerTree<W>) {
        let workspace = self.workspace;
        self.dispatch_focus(
            containers,
            |f, tree| {
                f.focus_up(tree);
                true
            },
            |t| {
                t.focus_up_or_right(workspace);
                true
            },
        );
    }

    pub fn focus_window_top(&mut self, containers: &mut ContainerTree<W>) {
        let workspace = self.workspace;
        self.dispatch_focus(
            containers,
            |f, tree| {
                f.focus_topmost(tree);
                true
            },
            |t| {
                t.focus_top(workspace);
                true
            },
        );
    }

    pub fn focus_window_bottom(&mut self, containers: &mut ContainerTree<W>) {
        let workspace = self.workspace;
        self.dispatch_focus(
            containers,
            |f, tree| {
                f.focus_bottommost(tree);
                true
            },
            |t| {
                t.focus_bottom(workspace);
                true
            },
        );
    }

    pub fn focus_window_down_or_top(&mut self, containers: &mut ContainerTree<W>) {
        if !self.focus_down(containers) {
            self.focus_window_top(containers);
        }
    }

    pub fn focus_window_up_or_bottom(&mut self, containers: &mut ContainerTree<W>) {
        if !self.focus_up(containers) {
            self.focus_window_bottom(containers);
        }
    }

    pub fn focus_up_no_wrap(&mut self, containers: &mut ContainerTree<W>) -> bool {
        let workspace = self.workspace;
        self.dispatch_focus(
            containers,
            |f, tree| f.focus_up_no_wrap(tree),
            |t| t.focus_up_no_wrap(workspace),
        )
    }

    pub fn focus_down_no_wrap(&mut self, containers: &mut ContainerTree<W>) -> bool {
        let workspace = self.workspace;
        self.dispatch_focus(
            containers,
            |f, tree| f.focus_down_no_wrap(tree),
            |t| t.focus_down_no_wrap(workspace),
        )
    }

    pub(super) fn focus_entry_from_output_direction(
        &mut self,
        containers: &mut ContainerTree<W>,
        direction: Direction,
    ) -> bool {
        if containers.has_fullscreen_window(self.workspace) {
            // Fullscreen workspace targets resolve to the inactive focus under
            // the fullscreen subtree. Keep tiling active as-is.
            self.runtime.layer = WorkspaceLayerState::Tiled;
            self.activate_tiling_content(containers);
            return true;
        }

        let Some((root_layout, child_count)) =
            containers.root_layout_and_child_count(self.workspace)
        else {
            return false;
        };
        if child_count == 0 {
            return false;
        }

        let use_edge = match direction {
            Direction::Left | Direction::Right => {
                matches!(root_layout, Layout::SplitH | Layout::Tabbed)
            }
            Direction::Up | Direction::Down => {
                matches!(root_layout, Layout::SplitV | Layout::Stacked)
            }
        };
        if !use_edge {
            // For non-parallel workspace layout, caller should use seat-level
            // inactive space.
            return false;
        }

        match direction {
            Direction::Left | Direction::Up => containers.focus_root_container_last(self.workspace),
            Direction::Right | Direction::Down => {
                containers.focus_root_container_first(self.workspace)
            }
        }
        self.runtime.layer = WorkspaceLayerState::Tiled;
        self.activate_tiling_content(containers);
        true
    }

    pub(super) fn focus_workspace_node(&mut self, containers: &mut ContainerTree<W>) {
        if containers.floating_root_count(self.workspace) == 0 {
            self.runtime.layer = WorkspaceLayerState::Tiled;
        } else {
            self.runtime.layer = WorkspaceLayerState::Floating;
        }
        let _ = containers.select_root_container(self.workspace);
    }

    fn focus_is_elevated(&self, containers: &ContainerTree<W>) -> bool {
        containers.workspace_is_selected(self.workspace)
    }

    /// Switch the active layer to tiling, with the new window as what commands are aimed at.
    ///
    /// Measured against sway 1.11: `focus parent` selects the workspace, but opening a
    /// window ends that — the next command goes to the window. The elevation records that
    /// the user asked for the workspace at some point; a window taking focus answers the
    /// question.
    fn activate_tiling_for_new_content(&mut self, containers: &mut ContainerTree<W>) {
        self.runtime.layer = WorkspaceLayerState::Tiled;
        containers.clear_workspace_selection(self.workspace);
    }

    fn activate_floating_for_new_content(&mut self, containers: &mut ContainerTree<W>) {
        self.runtime.layer = WorkspaceLayerState::Floating;
        containers.clear_workspace_selection(self.workspace);
    }

    #[cfg(test)]
    pub(super) fn is_floating_workspace_context_active(
        &self,
        containers: &ContainerTree<W>,
    ) -> bool {
        self.runtime.layer.floating_has_focus() && self.focus_is_elevated(containers)
    }

    /// Whether the workspace's real node is selected.
    #[cfg(test)]
    pub(super) fn tiling_targets_workspace(&self, containers: &ContainerTree<W>) -> bool {
        containers.workspace_is_selected(self.workspace)
    }

    #[cfg(test)]
    pub(super) fn is_tiling_workspace_context_active(&self, containers: &ContainerTree<W>) -> bool {
        !self.runtime.layer.floating_has_focus() && self.tiling_targets_workspace(containers)
    }

    pub fn focus_window_by_id(&mut self, containers: &mut ContainerTree<W>, id: &W::Id) -> bool {
        if containers.window_is_floating_in(self.workspace, id)
            && self.runtime.floating.focus_window_by_id(containers, id)
        {
            self.runtime.layer = WorkspaceLayerState::Floating;
            return true;
        }

        if containers.activate_window(self.workspace, id) {
            self.runtime.layer = WorkspaceLayerState::Tiled;
            self.activate_tiling_content(containers);
            return true;
        }

        false
    }

    pub fn move_left(&mut self, containers: &mut ContainerTree<W>) -> bool {
        let workspace = self.workspace;
        self.dispatch_move_directional(
            containers,
            Direction::Left,
            |f, tree| f.move_left(tree),
            |t| t.move_left(workspace),
        )
    }

    pub fn move_right(&mut self, containers: &mut ContainerTree<W>) -> bool {
        let workspace = self.workspace;
        self.dispatch_move_directional(
            containers,
            Direction::Right,
            |f, tree| f.move_right(tree),
            |t| t.move_right(workspace),
        )
    }

    pub fn move_root_container_to_first(&mut self, containers: &mut ContainerTree<W>) {
        let workspace = self.workspace;
        self.dispatch_tiling_root_reorder(containers, |t| t.move_root_container_to_first(workspace))
    }

    pub fn move_root_container_to_last(&mut self, containers: &mut ContainerTree<W>) {
        let workspace = self.workspace;
        self.dispatch_tiling_root_reorder(containers, |t| t.move_root_container_to_last(workspace))
    }

    pub fn move_root_container_to_index(
        &mut self,
        containers: &mut ContainerTree<W>,
        index: usize,
    ) {
        let workspace = self.workspace;
        self.dispatch_tiling_root_reorder(containers, |t| {
            t.move_root_container_to_index(workspace, index)
        })
    }

    pub fn move_down(&mut self, containers: &mut ContainerTree<W>) -> bool {
        let workspace = self.workspace;
        self.dispatch_move_directional(
            containers,
            Direction::Down,
            |f, tree| f.move_down(tree),
            |t| t.move_down(workspace),
        )
    }

    pub fn move_up(&mut self, containers: &mut ContainerTree<W>) -> bool {
        let workspace = self.workspace;
        self.dispatch_move_directional(
            containers,
            Direction::Up,
            |f, tree| f.move_up(tree),
            |t| t.move_up(workspace),
        )
    }

    pub fn consume_or_expel_window_left(
        &mut self,
        containers: &mut ContainerTree<W>,
        window: Option<&W::Id>,
    ) {
        let workspace = self.workspace;
        self.dispatch_for_window(
            containers,
            window,
            |f, tree| f.consume_or_expel_window_left(tree, window),
            |t| t.consume_or_expel_window_left(workspace, window),
        );
    }

    pub fn consume_or_expel_window_right(
        &mut self,
        containers: &mut ContainerTree<W>,
        window: Option<&W::Id>,
    ) {
        let workspace = self.workspace;
        self.dispatch_for_window(
            containers,
            window,
            |f, tree| f.consume_or_expel_window_right(tree, window),
            |t| t.consume_or_expel_window_right(workspace, window),
        );
    }

    pub fn consume_into_container(&mut self, containers: &mut ContainerTree<W>) {
        let workspace = self.workspace;
        self.dispatch_active_layer(
            containers,
            |f, tree| f.consume_into_container(tree),
            |t| t.consume_into_container(workspace),
        );
    }

    pub fn expel_from_container(&mut self, containers: &mut ContainerTree<W>) {
        let workspace = self.workspace;
        self.dispatch_active_layer(
            containers,
            |f, tree| f.expel_from_container(tree),
            |t| t.expel_from_container(workspace),
        );
    }

    pub fn toggle_container_tabbed(&mut self, containers: &mut ContainerTree<W>) {
        let workspace = self.workspace;
        self.dispatch_active_layer(
            containers,
            |f, tree| f.toggle_container_tabbed(tree),
            |t| t.toggle_container_tabbed(workspace),
        );
    }

    pub fn set_container_display(
        &mut self,
        containers: &mut ContainerTree<W>,
        display: ColumnDisplay,
    ) {
        let workspace = self.workspace;
        self.dispatch_active_layer(
            containers,
            |f, tree| f.set_container_display(tree, display),
            |t| t.set_container_display(workspace, display),
        );
    }

    /// sway's `move position center`, which is a floating-layer command.
    ///
    /// A tiled window has no position to set — it fills the slot its container gives it — so
    /// the tiling half is inert rather than inventing a meaning for centering inside a tree.
    /// sway says the same thing by refusing `move position` on a tiled container.
    pub fn center_window(&mut self, containers: &mut ContainerTree<W>, id: Option<&W::Id>) {
        if self.is_floating_target(containers, id) {
            self.runtime.floating.center_window(containers, id);
        }
    }

    pub fn toggle_width(&mut self, containers: &mut ContainerTree<W>, forwards: bool) {
        let workspace = self.workspace;
        self.dispatch_active_layer(
            containers,
            |f, tree| f.toggle_window_width(tree, None, forwards),
            |t| t.toggle_width(workspace, forwards),
        );
    }

    pub fn toggle_full_width(&mut self, containers: &mut ContainerTree<W>) {
        let workspace = self.workspace;
        // This gesture is defined for tiled resize edges. A floating window owns its geometry,
        // so applying a tree-width command to it would invent unrelated positioning semantics.
        self.dispatch_active_layer(
            containers,
            |_, _| {},
            |t| t.expand_root_container_to_available_width(workspace),
        );
    }

    pub fn set_root_container_width(
        &mut self,
        containers: &mut ContainerTree<W>,
        change: SizeChange,
    ) {
        let workspace = self.workspace;
        self.dispatch_active_layer(
            containers,
            |f, tree| f.set_window_width(tree, None, change, true),
            |t| t.set_root_container_width(workspace, change),
        );
    }

    pub fn set_window_width(
        &mut self,
        containers: &mut ContainerTree<W>,
        window: Option<&W::Id>,
        change: SizeChange,
    ) {
        self.resize_window(
            containers,
            window,
            ResizeRequest::Axis {
                axis: super::ResizeAxis::Horizontal,
                change,
            },
        );
    }

    pub fn set_window_height(
        &mut self,
        containers: &mut ContainerTree<W>,
        window: Option<&W::Id>,
        change: SizeChange,
    ) {
        self.resize_window(
            containers,
            window,
            ResizeRequest::Axis {
                axis: super::ResizeAxis::Vertical,
                change,
            },
        );
    }

    /// Route one semantic resize request by the addressed node, not merely by layer.
    pub fn resize_window(
        &mut self,
        containers: &mut ContainerTree<W>,
        window: Option<&W::Id>,
        request: ResizeRequest,
    ) {
        let target = match window {
            Some(id) => containers.window_key(id),
            None => Some(self.command_target_key(containers)),
        };
        let Some(target) = target else {
            return;
        };
        if target == self.workspace {
            // `focus parent` can elevate the seat to the workspace. sway then has no
            // handler-context container for `resize`; it must not fall back to the last
            // active floating root merely because that layer remains active for rendering.
            return;
        }

        if containers.is_floating_root(target) {
            self.runtime
                .floating
                .resize_window(containers, window, request);
        } else {
            // A descendant of a floating root is tiled inside that root. sway takes the
            // geometric floating path only for the root itself.
            containers.resize_node(target, request);
        }
    }

    pub fn reset_window_height(
        &mut self,
        containers: &mut ContainerTree<W>,
        window: Option<&W::Id>,
    ) {
        let workspace = self.workspace;
        self.dispatch_for_window(
            containers,
            window,
            |_, _| {},
            |t| t.reset_window_height(workspace, window),
        );
    }

    pub fn toggle_window_width(
        &mut self,
        containers: &mut ContainerTree<W>,
        window: Option<&W::Id>,
        forwards: bool,
    ) {
        let workspace = self.workspace;
        self.dispatch_for_window(
            containers,
            window,
            |f, tree| f.toggle_window_width(tree, window, forwards),
            |t| t.toggle_window_width(workspace, window, forwards),
        );
    }

    pub fn toggle_window_height(
        &mut self,
        containers: &mut ContainerTree<W>,
        window: Option<&W::Id>,
        forwards: bool,
    ) {
        let workspace = self.workspace;
        self.dispatch_for_window(
            containers,
            window,
            |f, tree| f.toggle_window_height(tree, window, forwards),
            |t| t.toggle_window_height(workspace, window, forwards),
        );
    }

    pub fn expand_root_container_to_available_width(&mut self, containers: &mut ContainerTree<W>) {
        let workspace = self.workspace;
        self.dispatch_active_layer(
            containers,
            |_, _| {},
            |t| t.expand_root_container_to_available_width(workspace),
        );
    }

    /// sway's `focus next|prev [sibling]` inside whichever tree branch owns the selection.
    pub fn focus_along_parent(
        &mut self,
        containers: &mut ContainerTree<W>,
        forward: bool,
        descend: bool,
    ) -> bool {
        match self.command_target(containers) {
            WorkspaceCommandTarget::Workspace => false,
            WorkspaceCommandTarget::Floating(_) => self
                .runtime
                .floating
                .focus_along_parent(containers, forward, descend),
            WorkspaceCommandTarget::Tiled(_) => {
                containers.focus_along_parent(self.workspace, forward, descend)
            }
        }
    }

    pub fn focus_parent(&mut self, containers: &mut ContainerTree<W>) {
        match self.command_target(containers) {
            WorkspaceCommandTarget::Workspace => {}
            WorkspaceCommandTarget::Floating(_) => {
                let _ = self.runtime.floating.focus_parent(containers);
            }
            WorkspaceCommandTarget::Tiled(_) => {
                let _ = containers.focus_parent(self.workspace);
            }
        }
    }

    pub fn focus_child(&mut self, containers: &mut ContainerTree<W>) {
        let workspace = self.workspace;
        let _ = self.dispatch_focus_from_workspace(
            containers,
            // sway descends through the workspace's active tiling child
            // (`seat_get_active_tiling_child`). Floating roots live in `ws->floating` and
            // are not candidates from workspace focus, which is why this lands in the tiled
            // side and takes the tiled epilogue with it.
            |tree| !tree.tiled_is_empty(workspace) && tree.focus_child(workspace),
            |floating, tree| floating.focus_child(tree),
            |tree| tree.focus_child(workspace),
        );
    }

    /// Route a split/layout command: workspace-level targets apply to the workspace layout,
    /// floating targets drop focus elevation unless the selected floating workspace container
    /// preserves it. Model rule: split/layout commands work in both floating and tiled branches.
    fn dispatch_layout(
        &mut self,
        containers: &mut ContainerTree<W>,
        workspace: impl FnOnce(&mut ContainerTree<W>),
        tiled: impl FnOnce(&mut ContainerTree<W>),
        floating: impl FnOnce(&mut FloatingRuntime<W>, &mut ContainerTree<W>),
    ) {
        match self.command_target(containers) {
            WorkspaceCommandTarget::Workspace => workspace(containers),
            WorkspaceCommandTarget::Floating(_) => {
                floating(&mut self.runtime.floating, containers);
            }
            WorkspaceCommandTarget::Tiled(_) => tiled(containers),
        }
        self.sync_active_layer_to_command_target(containers);
    }

    pub fn split_horizontal(&mut self, containers: &mut ContainerTree<W>) {
        let workspace = self.workspace;
        self.dispatch_layout(
            containers,
            |t| t.split_workspace_horizontal(workspace),
            |t| t.split_horizontal(workspace),
            |f, tree| f.split_horizontal(tree),
        );
    }

    pub fn split_vertical(&mut self, containers: &mut ContainerTree<W>) {
        let workspace = self.workspace;
        self.dispatch_layout(
            containers,
            |t| t.split_workspace_vertical(workspace),
            |t| t.split_vertical(workspace),
            |f, tree| f.split_vertical(tree),
        );
    }

    pub fn split_none(&mut self, containers: &mut ContainerTree<W>) {
        let workspace = self.workspace;
        self.dispatch_layout(
            containers,
            |_| {},
            |t| t.split_none(workspace),
            |f, tree| f.split_none(tree),
        );
    }

    /// `split toggle`, which sway does not implement as an operation of its own.
    ///
    /// `cmd_split` reads the layout of the parent of whatever the command is aimed at and
    /// runs `split h` when it is vertical and `split v` otherwise — including when there is
    /// no parent to read, which is the workspace itself. So this chooses, and everything
    /// after it is the ordinary split path, wrapping and all.
    pub fn split_toggle(&mut self, containers: &mut ContainerTree<W>) {
        let parent_is_vertical =
            containers.command_target_parent_layout(self.workspace) == Some(Layout::SplitV);
        if parent_is_vertical {
            self.split_horizontal(containers);
        } else {
            self.split_vertical(containers);
        }
    }

    pub fn set_layout_mode(&mut self, containers: &mut ContainerTree<W>, layout: Layout) {
        let workspace = self.workspace;
        self.dispatch_layout(
            containers,
            |t| {
                t.set_workspace_layout_mode(workspace, layout);
            },
            |t| t.set_layout_mode(workspace, layout),
            |f, tree| f.set_layout_mode(tree, layout),
        );
    }

    pub fn toggle_split_layout(&mut self, containers: &mut ContainerTree<W>) {
        let workspace = self.workspace;
        self.dispatch_layout(
            containers,
            |t| t.toggle_workspace_split_layout(workspace),
            |t| t.toggle_split_layout(workspace),
            |f, tree| f.toggle_split_layout(tree),
        );
    }

    pub fn toggle_layout_all(&mut self, containers: &mut ContainerTree<W>) {
        let workspace = self.workspace;
        self.dispatch_layout(
            containers,
            |t| t.toggle_workspace_layout_all(workspace),
            |t| t.toggle_layout_all(workspace),
            |f, tree| f.toggle_layout_all(tree),
        );
    }

    pub fn set_default_layout(&mut self, containers: &mut ContainerTree<W>) {
        let workspace = self.workspace;
        self.dispatch_layout(
            containers,
            |t| t.set_workspace_default_layout(workspace),
            |t| t.set_default_layout(workspace),
            |f, tree| f.set_default_layout(tree),
        );
    }

    pub(super) fn toggle_layout_cycle(
        &mut self,
        containers: &mut ContainerTree<W>,
        cycle: &[LayoutCycleEntry],
    ) {
        let workspace = self.workspace;
        self.dispatch_layout(
            containers,
            |t| t.toggle_workspace_layout_cycle(workspace, cycle),
            |t| t.toggle_layout_cycle(workspace, cycle),
            |f, tree| f.toggle_layout_cycle(tree, cycle),
        );
    }

    /// The zero-or-one window named by sway's workspace-wide fullscreen pointer.
    ///
    /// This stays a `Vec` for the existing inspection API; internally there is only one
    /// `Option<NodeKey>` and the id is derived from its live leaf.
    pub fn fullscreen_window_ids(&self, containers: &ContainerTree<W>) -> Vec<W::Id> {
        containers
            .fullscreen_representative_window_id(self.workspace)
            .cloned()
            .into_iter()
            .collect()
    }

    pub fn set_fullscreen(
        &mut self,
        containers: &mut ContainerTree<W>,
        window: &W::Id,
        is_fullscreen: bool,
    ) {
        if is_fullscreen {
            // A workspace has one fullscreen window. `container_set_fullscreen` disables the
            // one already there before setting the new one, and it looks it up through
            // `ws->fullscreen`, which spans both of the workspace's lists
            // (sway/tree/container.c:1375-1377). Without this, fullscreening across the two
            // sides leaves tiri with two.
            for id in self.fullscreen_window_ids(containers) {
                if &id != window {
                    self.set_fullscreen(containers, &id, false);
                }
            }
        }

        if containers.window_is_floating_in(self.workspace, window) {
            self.runtime
                .floating
                .set_fullscreen(containers, window, is_fullscreen);
            return;
        }

        containers.set_fullscreen(self.workspace, window, is_fullscreen);
    }

    pub fn toggle_fullscreen(&mut self, containers: &mut ContainerTree<W>, window: &W::Id) {
        if containers.window_is_floating_in(self.workspace, window) {
            let current = containers.floating_window_owns_fullscreen(self.workspace, window);
            self.set_fullscreen(containers, window, !current);
            return;
        }

        let tile = containers
            .tiles_in_workspace(self.workspace)
            .find(|tile| tile.window().id() == window)
            .unwrap();
        // Use space.is_fullscreen() as the source of truth instead of pending_sizing_mode(),
        // which updates asynchronously after animations complete.
        let current = containers.is_fullscreen(self.workspace, tile.window());
        self.set_fullscreen(containers, window, !current);
    }

    pub fn toggle_fullscreen_for_command(
        &mut self,
        containers: &mut ContainerTree<W>,
        _window: &W::Id,
    ) {
        // Resolve once to the tree object the command addresses. Floating and tiling are two
        // workspace branches, not two fullscreen semantics: a leaf owns client fullscreen,
        // while a container owns compositor-side fullscreen as the same stable NodeKey.
        let target = self.command_target_key(containers);

        if target == self.workspace {
            return;
        }
        if containers.is_leaf(target) {
            let Some(id) = containers
                .get_tile(target)
                .map(|tile| tile.window().id().clone())
            else {
                return;
            };
            self.toggle_fullscreen(containers, &id);
        } else {
            if containers.fullscreen_key(self.workspace) != Some(target) {
                // A previous leaf has protocol state to revoke. A previous container does
                // not; replacing the workspace pointer below is sufficient for it.
                if let Some(id) = containers
                    .fullscreen_leaf_window_id(self.workspace)
                    .cloned()
                {
                    self.set_fullscreen(containers, &id, false);
                }
            }
            containers.toggle_fullscreen_container(target);
        }
    }

    pub fn set_windowed_fullscreen(
        &mut self,
        containers: &mut ContainerTree<W>,
        window: &W::Id,
        is_fullscreen: bool,
    ) {
        self.set_fullscreen(containers, window, is_fullscreen);
    }

    pub fn toggle_window_floating(
        &mut self,
        containers: &mut ContainerTree<W>,
        id: Option<&W::Id>,
    ) {
        let Some(transfer) = self.resolve_floating_transfer(containers, id) else {
            return;
        };
        let animation_window = transfer.animation_window_id().cloned();
        let render_pos = animation_window.as_ref().and_then(|id| {
            self.tiles_with_render_positions(containers)
                .find(|(tile, _, _)| tile.window().id() == id)
                .map(|(_, pos, _)| pos)
        });
        let animate = match transfer {
            FloatingTransfer::Float(transfer) => self.execute_float_transfer(containers, transfer),
            FloatingTransfer::Unfloat(transfer) => {
                self.execute_unfloat_transfer(containers, transfer)
            }
        };

        if animate {
            if let Some(id) = animation_window {
                if let (Some(render_pos), Some((tile, new_render_pos))) = (
                    render_pos,
                    containers
                        .workspace_tiles_with_render_positions_mut(self.workspace, false)
                        .find(|(tile, _)| tile.window().id() == &id),
                ) {
                    tile.animate_move_from(render_pos - new_render_pos);
                }
            }
        }
    }

    fn resolve_floating_transfer(
        &mut self,
        containers: &mut ContainerTree<W>,
        requested_id: Option<&W::Id>,
    ) -> Option<FloatingTransfer<W::Id>> {
        let command_target = self.command_target(containers);
        let command_targets_workspace = matches!(command_target, WorkspaceCommandTarget::Workspace);
        let command_targets_tiling = matches!(command_target, WorkspaceCommandTarget::Tiled(_));

        if requested_id.is_none() && command_targets_workspace {
            // A selected workspace with no tiled children has nothing that can become floating.
            if containers.tiled_is_empty(self.workspace) {
                return None;
            }
        }

        let explicit_window = requested_id.is_some();
        let active_id = self.active_window(containers).map(|win| win.id().clone());
        let target_is_active = requested_id.is_none_or(|id| Some(id) == active_id.as_ref());
        let id = requested_id.cloned().or(active_id)?;
        let is_floating = containers.window_is_floating_in(self.workspace, &id);
        let inactive_tiling_reference = if is_floating {
            self.inactive_tiling_reference(containers)
        } else {
            None
        };

        if !explicit_window
            && target_is_active
            && command_targets_workspace
            && !containers.tiled_is_empty(self.workspace)
        {
            return Some(FloatingTransfer::Float(FloatTransfer::Workspace));
        }

        // Model rule: if a tiling container is selected (focus-parent semantics),
        // floating toggle targets that selected container even if floating focus mode
        // is currently active.
        if !explicit_window
            && target_is_active
            && command_targets_tiling
            && containers.selected_is_container(self.workspace)
        {
            return Some(FloatingTransfer::Float(FloatTransfer::SelectedContainer));
        }

        if is_floating {
            // `container_set_floating` asks `seat_get_focus_inactive_tiling` where the same
            // node lands (`sway/tree/container.c:1039-1057`). Its old parent is deliberately
            // not a restore target; sway removed that parent link when the node became floating.
            if !explicit_window {
                let was_workspace = self
                    .runtime
                    .floating
                    .active_container_is_workspace_floated(containers);
                let tiling_was_empty = containers.tiled_is_empty(self.workspace);
                let tiling_reference = if tiling_was_empty {
                    None
                } else {
                    inactive_tiling_reference
                };
                return Some(FloatingTransfer::Unfloat(UnfloatTransfer::Container {
                    id,
                    target_is_active,
                    tiling_reference,
                    was_workspace,
                    tiling_was_empty,
                }));
            }

            return Some(FloatingTransfer::Unfloat(UnfloatTransfer::Window {
                id,
                target_is_active,
                tiling_reference: inactive_tiling_reference,
            }));
        }

        Some(FloatingTransfer::Float(FloatTransfer::Window {
            id,
            target_is_active,
        }))
    }

    /// Execute a resolved tiling-to-floating transfer.
    ///
    /// The return value says whether the moved window should animate from its old render
    /// position. Whole-container transfers intentionally do not run that animation.
    fn execute_float_transfer(
        &mut self,
        containers: &mut ContainerTree<W>,
        transfer: FloatTransfer<W::Id>,
    ) -> bool {
        match transfer {
            FloatTransfer::Workspace => {
                let Some((subtree, rect)) =
                    containers.take_workspace_subtree_for_floating(self.workspace)
                else {
                    return false;
                };
                if !self
                    .runtime
                    .floating
                    .add_subtree(containers, subtree, rect, true)
                {
                    return false;
                }
                self.runtime.layer = WorkspaceLayerState::Floating;
                false
            }
            FloatTransfer::SelectedContainer => {
                let Some((subtree, rect)) = containers.take_selected_subtree(self.workspace) else {
                    return false;
                };
                if !self
                    .runtime
                    .floating
                    .add_subtree(containers, subtree, rect, false)
                {
                    return false;
                }
                self.runtime.layer = WorkspaceLayerState::Floating;
                false
            }
            FloatTransfer::Window {
                id,
                target_is_active,
            } => {
                let Some((subtree, rect)) =
                    containers.subtree_for_window_floating(self.workspace, &id)
                else {
                    return false;
                };
                if let Some(tile) = containers.get_tile_mut(subtree) {
                    tile.stop_move_animations();
                }

                if !self
                    .runtime
                    .floating
                    .add_subtree(containers, subtree, rect, false)
                {
                    return false;
                }
                // The floating side takes over either because the window that moved was the
                // active one, or because floating it emptied the tiled side and left nothing
                // for the active layer to point at.
                if target_is_active || containers.tiled_is_empty(self.workspace) {
                    self.runtime.layer = WorkspaceLayerState::Floating;
                }
                true
            }
        }
    }

    /// Execute a resolved floating-to-tiling transfer.
    ///
    /// An implicit command first targets the floating group. The window fallback preserves
    /// the old behaviour if that group vanished between resolution and execution.
    fn execute_unfloat_transfer(
        &mut self,
        containers: &mut ContainerTree<W>,
        transfer: UnfloatTransfer<W::Id>,
    ) -> bool {
        match transfer {
            UnfloatTransfer::Container {
                id,
                target_is_active,
                tiling_reference,
                was_workspace,
                tiling_was_empty,
            } => {
                if self.runtime.floating.unfloat_container(
                    containers,
                    &id,
                    tiling_reference.as_ref(),
                    was_workspace && tiling_was_empty,
                ) {
                    if target_is_active {
                        self.finish_active_unfloat(containers);
                    }
                    return false;
                }

                let unfloated = self.runtime.floating.unfloat_window(
                    containers,
                    &id,
                    tiling_reference.as_ref(),
                );
                if unfloated && target_is_active {
                    self.finish_active_unfloat(containers);
                }
                unfloated
            }
            UnfloatTransfer::Window {
                id,
                target_is_active,
                tiling_reference,
            } => {
                let unfloated = self.runtime.floating.unfloat_window(
                    containers,
                    &id,
                    tiling_reference.as_ref(),
                );
                if unfloated && target_is_active {
                    self.finish_active_unfloat(containers);
                }
                unfloated
            }
        }
    }

    fn finish_active_unfloat(&mut self, containers: &mut ContainerTree<W>) {
        self.runtime.layer = WorkspaceLayerState::Tiled;
        self.activate_tiling_content(containers);
    }

    pub fn scratchpad_window_id(&self, containers: &ContainerTree<W>) -> Option<W::Id> {
        containers
            .floating_tiles_in_workspace(self.workspace)
            .find(|tile| tile.is_scratchpad())
            .map(|tile| tile.window().id().clone())
    }

    /// Take a window out of the workspace to put it *away* in the scratchpad.
    ///
    /// Both of sway's hiding paths clear fullscreen before the container leaves the
    /// workspace: `root_scratchpad_add_container` for a window sent there for the first time,
    /// `root_scratchpad_hide` for a visible one going back. A hidden window that kept the
    /// state would come back out asking for fullscreen on a workspace that has since given
    /// its one pointer to somebody else.
    ///
    /// sway/tree/root.c:108-112, sway/tree/root.c:212-233
    pub(super) fn take_tile_for_hiding_in_scratchpad(
        &mut self,
        containers: &mut ContainerTree<W>,
        id: &W::Id,
    ) -> Option<ViewTransfer> {
        self.set_fullscreen(containers, id, false);
        self.take_tile_for_scratchpad(containers, id)
    }

    /// `container_fullscreen_disable` on whatever this workspace's one fullscreen pointer
    /// names, leaf or container.
    pub fn clear_fullscreen(&mut self, containers: &mut ContainerTree<W>) {
        let Some(scope) = containers.fullscreen_key(self.workspace) else {
            return;
        };

        if containers.is_leaf(scope) {
            if let Some(id) = containers
                .fullscreen_leaf_window_id(self.workspace)
                .cloned()
            {
                self.set_fullscreen(containers, &id, false);
            }
            return;
        }

        containers.toggle_fullscreen_container(scope);
    }

    pub(super) fn take_tile_for_scratchpad(
        &mut self,
        containers: &mut ContainerTree<W>,
        id: &W::Id,
    ) -> Option<ViewTransfer> {
        let transferred = self.take_tile_for_transfer(containers, id, Transaction::new());
        transferred.tile_mut(containers).set_scratchpad(true);
        transferred
            .tile_mut(containers)
            .window_mut()
            .set_floating(true);

        // `root_scratchpad_add_container`: a window that was already floating is put away
        // exactly as it is, size and position untouched. A tiled one is floated first, and
        // floating it is what decides its size — sway does that with
        // `container_floating_set_default_size` and `container_floating_move_to_center`, not
        // with the natural size a `floating enable` would have given it.
        //
        // sway/tree/root.c:114-119
        if !transferred.is_floating {
            transferred.tile_mut(containers).stop_move_animations();
            transferred.tile_mut(containers).clear_resize_animation();
            transferred.tile_mut(containers).floating_pos = None;

            let size = self.scratchpad_default_size(containers, transferred.tile(containers));
            transferred.tile_mut(containers).floating_window_size = Some(size);
            transferred
                .tile_mut(containers)
                .window_mut()
                .request_size_once(size, false);

            let working_area = containers.working_area(self.workspace);
            let size_f = Size::from((size.w as f64, size.h as f64));
            let pos = center_preferring_top_left_in_area(working_area, size_f);
            transferred.tile_mut(containers).floating_pos = Some(
                self.runtime
                    .floating
                    .logical_to_size_frac(working_area, pos),
            );

            let border_config = containers
                .options(self.workspace)
                .layout
                .border
                .merged_with(&transferred.tile(containers).window().rules().border);
            let bounds = compute_toplevel_bounds(border_config, working_area.size);
            let win = transferred.tile_mut(containers).window_mut();
            win.set_bounds(bounds);
            win.send_pending_configure();
            win.refresh();
        }

        Some(transferred)
    }

    /// Put a window away, at the back of the round-robin order.
    ///
    /// The order is the floating stack's: a hidden window goes on top, so the one hidden
    /// longest ago is at the bottom, and that is the one [`Self::next_scratchpad_window`]
    /// brings out. A queue, said with the ordering the workspace already has rather than with
    /// one beside it.
    pub(super) fn hide_in_scratchpad(
        &mut self,
        containers: &mut ContainerTree<W>,
        tile: IncomingView<W>,
    ) {
        self.add_scratchpad_tile(containers, tile, false);
    }

    /// The window `scratchpad show` would bring out next: the one hidden longest ago.
    pub fn next_scratchpad_window(&self, containers: &ContainerTree<W>) -> Option<W::Id> {
        containers
            .windows_in_workspace(self.workspace)
            .last()
            .map(|window| window.id().clone())
    }

    pub(super) fn add_scratchpad_tile(
        &mut self,
        containers: &mut ContainerTree<W>,
        mut tile: IncomingView<W>,
        activate: bool,
    ) {
        let tile_ref = containers
            .incoming_tile_mut(&mut tile)
            .expect("a scratchpad insertion must own a valid incoming view");
        tile_ref.set_scratchpad(true);
        tile_ref.window_mut().set_floating(true);
        self.enter_output_for_window(
            containers
                .incoming_tile(&tile)
                .expect("a scratchpad insertion must own a valid incoming view")
                .window(),
        );
        self.runtime.floating.add_tile(containers, tile, activate);

        if activate || containers.tiled_is_empty(self.workspace) {
            self.runtime.layer = WorkspaceLayerState::Floating;
        }
    }

    pub fn set_window_floating(
        &mut self,
        containers: &mut ContainerTree<W>,
        id: Option<&W::Id>,
        floating: bool,
    ) {
        if self.is_floating_target(containers, id) == floating {
            return;
        }

        self.toggle_window_floating(containers, id);
    }

    pub fn focus_floating(&mut self, containers: &mut ContainerTree<W>) {
        if !self.runtime.layer.floating_has_focus() {
            self.switch_focus_floating_tiling(containers);
        }
    }

    pub fn focus_tiling(&mut self, containers: &mut ContainerTree<W>) {
        if self.runtime.layer.floating_has_focus() {
            self.switch_focus_floating_tiling(containers);
        }
    }

    pub fn switch_focus_floating_tiling(&mut self, containers: &mut ContainerTree<W>) {
        if containers.floating_root_count(self.workspace) == 0
            || containers.tiled_is_empty(self.workspace)
        {
            return;
        }

        containers.clear_selection_context();
        let was_floating_active = self.runtime.layer.floating_has_focus();
        self.runtime.layer = if was_floating_active {
            WorkspaceLayerState::Tiled
        } else {
            WorkspaceLayerState::Floating
        };
        if !self.runtime.layer.floating_has_focus() {
            self.activate_tiling_content(containers);
        }
    }

    pub fn clear_selection_context(&mut self, containers: &mut ContainerTree<W>) {
        containers.clear_selection_context();
    }

    pub fn move_floating_window(
        &mut self,
        containers: &mut ContainerTree<W>,
        id: Option<&W::Id>,
        x: PositionChange,
        y: PositionChange,
        animate: bool,
    ) {
        if self.is_floating_target(containers, id) {
            self.runtime
                .floating
                .move_window(containers, id, x, y, animate);
        } else {
            // If the target tile isn't floating, set its stored floating position.
            let working_area = containers.working_area(self.workspace);
            let tile = if let Some(id) = id {
                containers
                    .tiled_tiles_mut(self.workspace)
                    .find(|tile| tile.window().id() == id)
                    .unwrap()
            } else if let Some(tile) = containers.active_tile_mut(self.workspace) {
                tile
            } else {
                return;
            };

            let pos = self
                .runtime
                .floating
                .stored_or_default_tile_pos(working_area, tile);

            // If there's no stored floating position, we can only set both components at once, not
            // adjust.
            let pos = pos.or_else(|| {
                (matches!(
                    x,
                    PositionChange::SetFixed(_) | PositionChange::SetProportion(_)
                ) && matches!(
                    y,
                    PositionChange::SetFixed(_) | PositionChange::SetProportion(_)
                ))
                .then_some(Point::default())
            });

            let Some(mut pos) = pos else {
                return;
            };

            let available_width = working_area.size.w;
            let available_height = working_area.size.h;
            let working_area_loc = working_area.loc;

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

            let pos = self
                .runtime
                .floating
                .logical_to_size_frac(working_area, pos);
            tile.floating_pos = Some(pos);
        }
    }

    #[cfg(test)]
    pub(super) fn has_window(&self, containers: &ContainerTree<W>, window: &W::Id) -> bool {
        let tree = containers;
        tree.window_key(window)
            .is_some_and(|key| tree.workspace_of(key) == Some(self.workspace))
    }

    pub fn tiles_with_render_positions<'a>(
        &'a self,
        containers: &'a ContainerTree<W>,
    ) -> impl Iterator<Item = (&'a Tile<W>, Point<f64, Logical>, bool)> + 'a {
        let fullscreen_scope = containers.floating_fullscreen_key(self.workspace);
        let tiled = containers
            .tiled_tiles_with_render_positions(self.workspace)
            .map(move |(tile, pos, visible)| (tile, pos, visible && fullscreen_scope.is_none()));

        let floating = containers.floating_tiles_with_render_positions_keyed(self.workspace);
        let visible = self.is_floating_visible(containers);
        let tree = containers;
        let floating = floating.map(move |(key, tile, pos)| {
            let in_scope = fullscreen_scope.is_none_or(|scope| tree.is_descendant(key, scope));
            (tile, pos, visible && in_scope)
        });

        floating.chain(tiled)
    }

    pub fn active_window_visual_rectangle(
        &self,
        containers: &ContainerTree<W>,
    ) -> Option<Rectangle<f64, Logical>> {
        if self.runtime.layer.floating_has_focus() {
            containers.active_floating_window_visual_rectangle(self.workspace)
        } else {
            containers.active_tile_visual_rectangle(self.workspace)
        }
    }

    pub fn popup_target_rect(
        &self,
        containers: &ContainerTree<W>,
        window: &W::Id,
    ) -> Option<Rectangle<f64, Logical>> {
        if containers.window_is_floating_in(self.workspace, window) {
            containers.floating_popup_target_rect(self.workspace, window)
        } else {
            containers.popup_target_rect(self.workspace, window)
        }
    }

    pub fn render_tiling<R: NiriRenderer>(
        &self,
        containers: &ContainerTree<W>,
        ctx: RenderCtx<R>,
        xray_pos: XrayPos,
        focus_ring: bool,
        layer: RenderLayer,
        push: &mut dyn FnMut(WorkspaceRenderElement<R>),
    ) {
        if containers.floating_fullscreen_key(self.workspace).is_some() {
            return;
        }
        let tiling_focus_ring = focus_ring && !self.runtime.layer.floating_has_focus();
        containers.render(
            self.workspace,
            &self.runtime.tiled.closing_windows,
            ctx,
            xray_pos,
            BranchRenderParams {
                has_focus: self.runtime.layer_has_focus(LayoutLayer::Tiled),
                focus_ring: tiling_focus_ring,
                layer,
            },
            &mut |elem| push(elem.into()),
        );
    }

    pub fn render_tiling_as_offscreen<R: NiriRenderer>(
        &self,
        containers: &ContainerTree<W>,
        renderer: &mut GlesRenderer,
        target: RenderTarget,
        focus_ring: bool,
        push: &mut dyn FnMut(WorkspaceRenderElement<R>),
    ) {
        if containers.floating_fullscreen_key(self.workspace).is_some() {
            return;
        }
        let tiling_focus_ring = focus_ring && !self.runtime.layer.floating_has_focus();
        if let Some(elem) = containers.render_as_offscreen(
            self.workspace,
            &self.runtime.tiled.closing_windows,
            renderer,
            target,
            BranchRenderParams {
                has_focus: self.runtime.layer_has_focus(LayoutLayer::Tiled),
                focus_ring: tiling_focus_ring,
                layer: RenderLayer::Normal,
            },
        ) {
            push(elem.into());
        }
    }

    pub fn render_floating<R: NiriRenderer>(
        &self,
        containers: &ContainerTree<W>,
        ctx: RenderCtx<R>,
        xray_pos: XrayPos,
        focus_ring: bool,
        layer: RenderLayer,
        push: &mut dyn FnMut(WorkspaceRenderElement<R>),
    ) {
        if !self.is_floating_visible(containers) && layer.is_normal() {
            return;
        }

        let view_rect = Rectangle::from_size(containers.view_size(self.workspace));
        let floating_focus_ring = focus_ring && self.runtime.layer.floating_has_focus();
        self.runtime.floating.render(
            containers,
            ctx,
            xray_pos,
            view_rect,
            BranchRenderParams {
                has_focus: self.runtime.layer_has_focus(LayoutLayer::Floating),
                focus_ring: floating_focus_ring,
                layer,
            },
            &mut |elem| push(elem.into()),
        );
    }

    pub fn render_shadow<R: NiriRenderer>(
        &self,
        renderer: &mut R,
        push: &mut dyn FnMut(ShadowRenderElement),
    ) {
        self.shadow.render(renderer, Point::from((0., 0.)), push);
    }

    pub fn render_background(&self) -> SolidColorRenderElement {
        SolidColorRenderElement::from_buffer(
            &self.background_buffer,
            Point::new(0., 0.),
            1.,
            Kind::Unspecified,
        )
    }

    pub fn render_above_top_layer(&self, containers: &ContainerTree<W>) -> bool {
        containers.render_above_top_layer(self.workspace)
            || containers
                .floating_fullscreen_key(self.workspace)
                .is_some_and(|key| {
                    containers.container_info(key).is_some()
                        || containers
                            .get_tile(key)
                            .is_some_and(|tile| tile.window().sizing_mode().is_fullscreen())
                })
    }

    pub fn is_floating_visible(&self, containers: &ContainerTree<W>) -> bool {
        // If the focus is on a fullscreen tiling window, hide the floating windows.
        matches!(
            self.runtime.layer,
            WorkspaceLayerState::Floating | WorkspaceLayerState::TiledWithFloatingRaised
        ) || containers.floating_fullscreen_key(self.workspace).is_some()
            || !self.render_above_top_layer(containers)
    }

    pub fn store_unmap_snapshot_if_empty(
        &mut self,
        containers: &mut ContainerTree<W>,
        renderer: &mut GlesRenderer,
        xray: Option<&mut Xray>,
        xray_has_blocked_out_layers: bool,
        xray_pos: XrayPos,
        window: &W::Id,
    ) {
        let view_size = containers.view_size(self.workspace);
        for (tile, tile_pos) in
            containers.workspace_tiles_with_render_positions_mut(self.workspace, false)
        {
            if tile.window().id() == window {
                let view_pos = Point::from((-tile_pos.x, -tile_pos.y));
                let view_rect = Rectangle::new(view_pos, view_size);
                tile.update_render_elements(
                    false,
                    false,
                    false,
                    crate::layout::focus_ring::FocusRingEdges::all(),
                    None,
                    view_rect,
                );
                let xray_pos = xray_pos.offset(tile_pos);
                tile.store_unmap_snapshot_if_empty(
                    renderer,
                    xray,
                    xray_has_blocked_out_layers,
                    xray_pos,
                );
                return;
            }
        }
    }

    pub fn clear_unmap_snapshot(&mut self, containers: &mut ContainerTree<W>, window: &W::Id) {
        for tile in containers.tiles_mut_in_workspace(self.workspace) {
            if tile.window().id() == window {
                let _ = tile.take_unmap_snapshot();
                return;
            }
        }
    }

    pub fn start_close_animation_for_window(
        &mut self,
        containers: &mut ContainerTree<W>,
        renderer: &mut GlesRenderer,
        window: &W::Id,
        blocker: TransactionBlocker,
    ) {
        if containers.window_is_floating_in(self.workspace, window) {
            self.runtime
                .floating
                .start_close_animation_for_window(containers, renderer, window, blocker);
        } else {
            containers.start_close_animation_for_window(
                self.workspace,
                &mut self.runtime.tiled.closing_windows,
                renderer,
                window,
                blocker,
            );
        }
    }

    pub fn start_close_animation_for_tile(
        &mut self,
        containers: &mut ContainerTree<W>,
        renderer: &mut GlesRenderer,
        snapshot: TileRenderSnapshot,
        tile_size: Size<f64, Logical>,
        tile_pos: Point<f64, Logical>,
        blocker: TransactionBlocker,
    ) {
        self.runtime.floating.start_close_animation_for_tile(
            containers, renderer, snapshot, tile_size, tile_pos, blocker,
        );
    }

    pub fn start_open_animation(&mut self, containers: &mut ContainerTree<W>, id: &W::Id) -> bool {
        containers.start_open_animation(self.workspace, id)
            || self.runtime.floating.start_open_animation(containers, id)
    }

    pub fn window_under<'a>(
        &'a self,
        containers: &'a ContainerTree<W>,
        pos: Point<f64, Logical>,
    ) -> Option<(&'a W, HitType)> {
        if self.is_floating_visible(containers) {
            if let Some(rv) = self.runtime.floating.window_under(containers, pos) {
                return Some(rv);
            }
        }

        containers.window_under(self.workspace, pos)
    }

    pub fn resize_edges_under(
        &mut self,
        containers: &mut ContainerTree<W>,
        pos: Point<f64, Logical>,
    ) -> Option<ResizeEdge> {
        self.resize_hit_under(containers, pos).map(|hit| hit.edges)
    }

    pub fn resize_hit_under(
        &mut self,
        containers: &mut ContainerTree<W>,
        pos: Point<f64, Logical>,
    ) -> Option<ResizeHit<W::Id>> {
        if containers.is_active_pending_fullscreen(self.workspace) {
            return None;
        }

        if self.is_floating_visible(containers) {
            match self.runtime.floating.resize_hit_under(containers, pos) {
                FloatingResizeResult::Hit(hit) => {
                    let cursor = if !hit.external_edges.is_empty() {
                        external_resize_cursor_icon(hit.external_edges)
                    } else {
                        hit.edges.cursor_icon()
                    };
                    return Some(ResizeHit {
                        window: hit.window,
                        edges: hit.edges,
                        cursor,
                        is_floating: true,
                    });
                }
                FloatingResizeResult::Blocked => return None,
                FloatingResizeResult::None => {}
            }
            if self.runtime.layer.floating_has_focus() {
                return None;
            }
        }

        containers.resize_hit_under(self.workspace, pos)
    }

    pub fn descendants_added(&mut self, containers: &mut ContainerTree<W>, id: &W::Id) -> bool {
        self.runtime.floating.descendants_added(containers, id)
    }

    pub fn update_window(
        &mut self,
        containers: &mut ContainerTree<W>,
        window: &W::Id,
        serial: Option<Serial>,
    ) {
        if !self
            .runtime
            .floating
            .update_window(containers, window, serial)
        {
            containers.update_window(self.workspace, window, serial);
        }
    }

    pub fn refresh(
        &mut self,
        containers: &mut ContainerTree<W>,
        is_active: bool,
        is_focused: bool,
    ) {
        let _span = tracy_client::span!("Workspace::refresh");
        containers.refresh(
            self.workspace,
            &self.runtime.tiled.interactive_resize,
            is_active && !self.runtime.layer.floating_has_focus(),
            is_focused,
        );
        self.runtime.floating.refresh(
            containers,
            is_active && self.runtime.layer.floating_has_focus(),
            is_focused,
        );
    }

    pub fn is_urgent(&self, containers: &ContainerTree<W>) -> bool {
        containers
            .windows_in_workspace(self.workspace)
            .any(|win| win.is_urgent())
    }

    pub fn activate_window(&mut self, containers: &mut ContainerTree<W>, window: &W::Id) -> bool {
        if self.runtime.floating.activate_window(containers, window) {
            self.runtime.layer = WorkspaceLayerState::Floating;
            true
        } else if containers.activate_window(self.workspace, window) {
            self.runtime.layer = WorkspaceLayerState::Tiled;
            self.activate_tiling_content(containers);
            true
        } else {
            false
        }
    }

    pub fn activate_window_without_raising(
        &mut self,
        containers: &mut ContainerTree<W>,
        window: &W::Id,
    ) -> bool {
        if self
            .runtime
            .floating
            .activate_window_without_raising(containers, window)
        {
            self.runtime.layer = WorkspaceLayerState::Floating;
            true
        } else if containers.activate_window(self.workspace, window) {
            if self.runtime.layer == WorkspaceLayerState::Floating {
                self.runtime.layer = WorkspaceLayerState::TiledWithFloatingRaised;
            }
            self.activate_tiling_content(containers);
            true
        } else {
            false
        }
    }

    pub(super) fn tiling_insert_position(
        &self,
        containers: &ContainerTree<W>,
        pos: Point<f64, Logical>,
    ) -> InsertPosition {
        containers.insert_position(self.workspace, pos)
    }

    pub(super) fn insert_hint_area(
        &self,
        containers: &ContainerTree<W>,
        position: &InsertPosition,
    ) -> Option<Rectangle<f64, Logical>> {
        containers.insert_hint_area(self.workspace, position)
    }

    pub fn interactive_resize_begin(
        &mut self,
        containers: &mut ContainerTree<W>,
        window: W::Id,
        edges: ResizeEdge,
    ) -> bool {
        if containers.window_is_floating_in(self.workspace, &window) {
            self.runtime
                .floating
                .interactive_resize_begin(containers, window, edges)
        } else {
            containers.interactive_resize_begin(
                self.workspace,
                &mut self.runtime.tiled.interactive_resize,
                window,
                edges,
            )
        }
    }

    pub fn interactive_resize_begin_at(
        &mut self,
        containers: &mut ContainerTree<W>,
        window: W::Id,
        edges: ResizeEdge,
        pos: Point<f64, Logical>,
    ) -> bool {
        if containers.window_is_floating_in(self.workspace, &window) {
            self.runtime
                .floating
                .interactive_resize_begin(containers, window, edges)
        } else {
            containers.interactive_resize_begin_at(
                self.workspace,
                &mut self.runtime.tiled.interactive_resize,
                window,
                edges,
                pos,
            )
        }
    }

    pub fn interactive_resize_update(
        &mut self,
        containers: &mut ContainerTree<W>,
        window: &W::Id,
        delta: Point<f64, Logical>,
    ) -> bool {
        if containers.window_is_floating_in(self.workspace, window) {
            self.runtime
                .floating
                .interactive_resize_update(containers, window, delta)
        } else {
            containers.interactive_resize_update(
                self.workspace,
                &self.runtime.tiled.interactive_resize,
                window,
                delta,
            )
        }
    }

    pub fn interactive_resize_end(
        &mut self,
        containers: &mut ContainerTree<W>,
        window: Option<&W::Id>,
    ) {
        if let Some(window) = window {
            if containers.window_is_floating_in(self.workspace, window) {
                self.runtime.floating.interactive_resize_end(Some(window));
            } else {
                containers.interactive_resize_end(
                    &mut self.runtime.tiled.interactive_resize,
                    Some(window),
                );
            }
        } else {
            self.runtime.floating.interactive_resize_end(None);
            containers.interactive_resize_end(&mut self.runtime.tiled.interactive_resize, None);
        }
    }

    pub fn floating_is_active(&self) -> bool {
        self.runtime.layer.floating_has_focus()
    }

    #[cfg(test)]
    pub(super) fn layer_has_focus(&self, layer: LayoutLayer) -> bool {
        self.runtime.layer_has_focus(layer)
    }

    pub fn floating_logical_to_size_frac(
        &self,
        containers: &ContainerTree<W>,
        logical_pos: Point<f64, Logical>,
    ) -> Point<f64, SizeFrac> {
        self.runtime
            .floating
            .logical_to_size_frac(containers.working_area(self.workspace), logical_pos)
    }

    pub(super) fn floating_container_allows_splits(
        &self,
        containers: &ContainerTree<W>,
        id: &W::Id,
    ) -> bool {
        self.runtime
            .floating
            .container_allows_splits(containers, id)
    }

    pub(super) fn floating_container_pos(
        &self,
        containers: &ContainerTree<W>,
        id: &W::Id,
    ) -> Option<Point<f64, Logical>> {
        self.runtime.floating.container_pos(containers, id)
    }

    pub(super) fn move_floating_container_for_window_to(
        &mut self,
        containers: &mut ContainerTree<W>,
        id: &W::Id,
        pos: Point<f64, Logical>,
    ) -> bool {
        self.runtime
            .floating
            .move_container_for_window_to(containers, id, pos, false)
    }

    pub fn layout_config(&self) -> Option<&tiri_config::LayoutPart> {
        self.layout_config.as_ref()
    }

    /// This workspace's view of the shared tree.
    #[cfg(test)]
    pub fn container_tree<'a>(&self, containers: &'a ContainerTree<W>) -> WorkspaceTreeView<'a, W> {
        WorkspaceTreeView {
            containers,
            workspace: self.workspace,
        }
    }

    #[cfg(test)]
    pub fn floating<'a>(&'a self, containers: &'a ContainerTree<W>) -> FloatingTestView<'a, W> {
        FloatingTestView {
            containers,
            workspace: self.workspace,
        }
    }

    #[cfg(test)]
    pub fn debug_active_floating_wrapper_selected(&self, containers: &ContainerTree<W>) -> bool {
        let Some(id) = containers.active_floating_window_id(self.workspace) else {
            return false;
        };
        let Some(key) = containers.window_key(&id) else {
            return false;
        };
        containers.selected_container_key() == Some(containers.branch_root(key))
    }

    #[cfg(test)]
    pub fn debug_command_context(&self, containers: &ContainerTree<W>) -> &'static str {
        match self.command_target(containers) {
            WorkspaceCommandTarget::Workspace => "workspace",
            WorkspaceCommandTarget::Floating(_) => "floating",
            WorkspaceCommandTarget::Tiled(_) => "tiling",
        }
    }

    #[cfg(test)]
    pub fn debug_command_target(&self, containers: &ContainerTree<W>) -> &'static str {
        match self.command_target(containers) {
            WorkspaceCommandTarget::Workspace => "workspace",
            WorkspaceCommandTarget::Floating(key) if containers.is_leaf(key) => "floating_window",
            WorkspaceCommandTarget::Floating(_) => "floating_container",
            WorkspaceCommandTarget::Tiled(key) if containers.is_leaf(key) => "tiling_window",
            WorkspaceCommandTarget::Tiled(_) => "tiling_container",
        }
    }

    #[cfg(test)]
    pub fn debug_floating_workspace_context(&self, containers: &ContainerTree<W>) -> bool {
        self.is_floating_workspace_context_active(containers)
    }

    #[cfg(test)]
    pub fn debug_workspace_layout(&self, containers: &ContainerTree<W>) -> Layout {
        containers.debug_workspace_layout(self.workspace)
    }

    pub fn verify_invariants(&self, containers: &ContainerTree<W>, move_win_id: Option<&W::Id>) {
        use approx::assert_abs_diff_eq;

        // The floating side holds the same key by construction and nothing reassigns either.
        // Asserted rather than removed: see the field on `FloatingRuntime` for why threading it
        // would cost more than the duplication does.
        assert_eq!(
            self.workspace,
            self.runtime.floating.workspace(),
            "a workspace's floating side must address that workspace"
        );

        // sway's `ws->fullscreen` is one pointer for the whole workspace.
        let fullscreen = self.fullscreen_window_ids(containers);
        assert!(
            fullscreen.len() <= 1,
            "a workspace has at most one fullscreen window, found {fullscreen:?}"
        );
        let pending_fullscreen: Vec<_> = containers
            .windows_in_workspace(self.workspace)
            .filter(|window| window.pending_sizing_mode().is_fullscreen())
            .map(|window| window.id().clone())
            .collect();
        assert!(
            pending_fullscreen.len() <= 1,
            "a workspace cannot request fullscreen for two clients: {pending_fullscreen:?}"
        );
        if let Some(id) = pending_fullscreen.first() {
            assert!(
                containers.window_owns_fullscreen(self.workspace, id),
                "a pending fullscreen client must be the workspace fullscreen owner"
            );
        }

        let scale = self.scale.fractional_scale();
        assert!(scale > 0.);
        assert!(scale.is_finite());

        let options = Options::clone(&self.base_options)
            .with_merged_layout(self.layout_config.as_ref())
            .adjusted_for_scale(scale);
        assert_eq!(
            &**containers.options(self.workspace),
            &options,
            "options must be base options adjusted for scale"
        );

        assert!(containers.view_size(self.workspace).w > 0.);
        assert!(containers.view_size(self.workspace).h > 0.);

        assert_eq!(
            self.background_buffer.size(),
            containers.view_size(self.workspace)
        );
        assert_eq!(
            self.background_buffer.color().components(),
            options.layout.background_color.to_array_unpremul(),
        );

        assert_eq!(
            containers.working_area(self.workspace),
            containers.parent_area(self.workspace)
        );
        containers.verify_invariants(self.workspace);

        self.runtime.floating.verify_runtime_invariants(containers);

        if containers.floating_root_count(self.workspace) == 0 {
            assert!(
                !self.runtime.layer.floating_has_focus(),
                "when floating is empty it must never be active"
            );
        } else if containers.tiled_is_empty(self.workspace) {
            assert!(
                self.runtime.layer.floating_has_focus(),
                "when tiling is empty but floating isn't, floating should be active"
            );
        }

        for (tile, tile_pos, visible) in self.tiles_with_render_positions(containers) {
            if Some(tile.window().id()) != move_win_id {
                assert_eq!(tile.interactive_move_offset, Point::from((0., 0.)));
            }

            let rounded_pos = tile_pos.to_physical_precise_round(scale).to_logical(scale);

            // Tile positions must be rounded to physical pixels.
            assert_abs_diff_eq!(tile_pos.x, rounded_pos.x, epsilon = 1e-5);
            assert_abs_diff_eq!(tile_pos.y, rounded_pos.y, epsilon = 1e-5);

            if let Some(alpha) = &tile.alpha_animation {
                let anim = &alpha.anim;
                if visible {
                    assert_eq!(anim.to(), 1., "visible tiles can animate alpha only to 1");
                }

                assert!(
                    !alpha.hold_after_done,
                    "tiles in the layout cannot have held alpha animation"
                );
            }
        }
    }
}

impl<W: LayoutElement> Workspace<W> {
    pub(crate) fn layout_tree(&self, containers: &ContainerTree<W>) -> Option<LayoutTreeNode> {
        if self.runtime.layer.floating_has_focus() {
            containers.layout_tree_unfocused(self.workspace)
        } else {
            containers.layout_tree(self.workspace)
        }
    }
}

pub(super) fn compute_working_area(
    output: &Output,
    struts: Struts,
    scale: f64,
) -> Rectangle<f64, Logical> {
    inset_by_struts(
        layer_map_for_output(output).non_exclusive_zone().to_f64(),
        struts,
        scale,
    )
}

/// Reserve the configured struts at the edges of an area.
///
/// Layer-shell exclusive zones are already out of `area`; struts are what the user reserves on
/// top of that. They may be negative, which is how one asks for windows to extend under a bar
/// that reserved more room than it draws in.
pub(super) fn inset_by_struts(
    area: Rectangle<f64, Logical>,
    struts: Struts,
    scale: f64,
) -> Rectangle<f64, Logical> {
    let mut area = area;

    area.size.w = f64::max(0., area.size.w - struts.left.0 - struts.right.0);
    area.loc.x += struts.left.0;

    area.size.h = f64::max(0., area.size.h - struts.top.0 - struts.bottom.0);
    area.loc.y += struts.top.0;

    // A strut can be fractional, so round the origin back onto a physical pixel and take the
    // rounding out of the size rather than letting the layout start half inside one.
    let loc = area.loc.to_physical_precise_ceil(scale).to_logical(scale);
    let mut diff = (loc - area.loc).to_size();
    diff.w = f64::min(area.size.w, diff.w);
    diff.h = f64::min(area.size.h, diff.h);

    area.size -= diff;
    area.loc = loc;

    area
}

fn compute_workspace_shadow_config(
    config: tiri_config::WorkspaceShadow,
    view_size: Size<f64, Logical>,
) -> tiri_config::Shadow {
    // Gaps between workspaces are a multiple of the view height, so shadow settings should also be
    // normalized to the view height to prevent them from overlapping on lower resolutions.
    let norm = view_size.h / 1080.;

    let mut config = tiri_config::Shadow::from(config);
    config.softness *= norm;
    config.spread *= norm;
    config.offset.x.0 *= norm;
    config.offset.y.0 *= norm;

    config
}
