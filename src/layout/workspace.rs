use std::cmp::max;
use std::marker::PhantomData;
use std::rc::Rc;

use smithay::backend::renderer::element::Kind;
use smithay::backend::renderer::gles::GlesRenderer;
use smithay::desktop::{layer_map_for_output, Window};
use smithay::output::Output;
use smithay::reexports::wayland_protocols::xdg::shell::server::xdg_toplevel;
use smithay::utils::{Logical, Point, Rectangle, Serial, Size, Transform};
use smithay::wayland::compositor::with_states;
use smithay::wayland::shell::xdg::SurfaceCachedState;
use tiri_config::utils::MergeWith as _;
use tiri_config::{CornerRadius, PresetSize, Struts, Workspace as WorkspaceConfig};
use tiri_ipc::{LayoutTreeNode, PositionChange, SizeChange};

use super::container::{
    Direction, InactiveTilingReference, IncomingView, InsertParentInfo, LayoutCommand, LayoutLayer,
    LayoutMode, NewView, NodeKey, OutputKey, WorkspaceCommandTarget,
};
use super::container_tree::{ContainerTree, ContainerTreeRenderElement, RootTilingSubtree};
pub(super) use super::floating::FloatingRoots;
use super::floating::{
    compute_toplevel_bounds, FloatingRenderElement, FloatingResizeResult, FloatingRuntime,
};
use super::tile::{Tile, TileRenderSnapshot};
use super::{
    ActivateWindow, BranchRenderParams, FocusCommand, HitType, InsertPosition,
    InteractiveResizeData, LayoutElement, Options, ResizeAxis, ResizeHit, ResizeRequest,
    RootPosition, SizeCommand, SizeFrac, ViewTransfer,
};
use crate::layout::RenderLayer;
use crate::niri_render_elements;
use crate::render_helpers::offscreen::OffscreenRenderElement;
use crate::render_helpers::renderer::NiriRenderer;
use crate::render_helpers::shadow::ShadowRenderElement;
use crate::render_helpers::solid_color::SolidColorRenderElement;
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
pub(super) use runtime::{FloatingRuntimeState, WorkspaceRuntime};
mod state;
pub use state::{OutputId, WorkspaceId};
pub(super) use state::{WorkspaceIdentity, WorkspaceLifetime, WorkspaceProperties};
mod transfer;
use transfer::{FloatTransfer, FloatingTransfer, UnfloatTransfer};

/// A workspace: a handle to its node in the layout's tree.
///
/// Everything about the workspace is on the node: what it *is* — its name, its outputs, its
/// config ([`WorkspaceProperties`]) —, everything it holds, and what it is doing right now
/// ([`WorkspaceRuntime`]), and which output it is on, in which place. A handle is a node key
/// with a type, and as cheap to copy.
pub struct Workspace<W: LayoutElement> {
    /// This workspace's node in the layout's tree.
    workspace: NodeKey,
    _element: PhantomData<fn() -> W>,
}

impl<W: LayoutElement> Clone for Workspace<W> {
    fn clone(&self) -> Self {
        *self
    }
}

impl<W: LayoutElement> Copy for Workspace<W> {}

impl<W: LayoutElement> std::fmt::Debug for Workspace<W> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_tuple("Workspace").field(&self.workspace).finish()
    }
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

    /// A workspace sized and configured for `output`, but on no output until the output's
    /// monitor attaches it where it belongs among its workspaces.
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

        let transform = output.current_transform();
        let workspace = containers.add_workspace(
            view_size,
            working_area,
            options.clone(),
            WorkspaceProperties {
                identity,
                lifetime: WorkspaceLifetime::Persistent,
                original_output,
                output_scale: scale,
                transform,
                layout_config,
                base_options,
            },
        );

        Self::of(workspace)
    }

    /// The handle for a workspace node.
    pub(super) fn of(workspace: NodeKey) -> Self {
        Self {
            workspace,
            _element: PhantomData,
        }
    }

    /// The floating side of this workspace.
    fn floating_side(&self) -> FloatingRuntime<W> {
        FloatingRuntime::new(self.workspace)
    }

    fn runtime<'a>(&self, containers: &'a ContainerTree<W>) -> &'a WorkspaceRuntime<W> {
        containers.workspace_runtime(self.workspace)
    }

    fn runtime_mut<'a>(&self, containers: &'a mut ContainerTree<W>) -> &'a mut WorkspaceRuntime<W> {
        containers.workspace_runtime_mut(self.workspace)
    }

    /// The scratchpad: a workspace that is never on an output.
    ///
    /// sway's `__i3_scratch` is an ordinary workspace on a hidden output, which is what makes
    /// a hidden window an ordinary window — arranged, configured, and in step with its client
    /// rather than waiting to be told what it is on the way back out.
    pub fn new_scratchpad(containers: &mut ContainerTree<W>, options: Rc<Options>) -> Self {
        // The scratchpad is the tree's first root, created like any other workspace: sway's
        // `__i3_scratch` is an ordinary workspace on a hidden output, not a special store.
        let workspace = Self::new_with_config_no_outputs(containers, None, options);
        containers
            .workspace_properties_mut(workspace.workspace)
            .identity = WorkspaceIdentity::Anonymous;
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
            options.clone(),
            WorkspaceProperties {
                identity,
                lifetime: WorkspaceLifetime::Persistent,
                original_output,
                output_scale: scale,
                transform: Transform::Normal,
                layout_config,
                base_options,
            },
        );

        Self::of(workspace)
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
        WorkspaceId::of(self.workspace)
    }

    /// What this workspace is: its name, its outputs, its own config.
    pub(super) fn properties<'a>(
        &self,
        containers: &'a ContainerTree<W>,
    ) -> &'a WorkspaceProperties {
        containers.workspace_properties(self.workspace)
    }

    fn properties_mut<'a>(
        &self,
        containers: &'a mut ContainerTree<W>,
    ) -> &'a mut WorkspaceProperties {
        containers.workspace_properties_mut(self.workspace)
    }

    pub fn name<'a>(&self, containers: &'a ContainerTree<W>) -> Option<&'a String> {
        self.properties(containers).name()
    }

    pub(super) fn numeric_number(&self, containers: &ContainerTree<W>) -> Option<u32> {
        self.properties(containers).numeric_number()
    }

    pub(super) fn has_persistent_identity(&self, containers: &ContainerTree<W>) -> bool {
        self.properties(containers).has_persistent_identity()
    }

    pub(super) fn make_identity_persistent(&self, containers: &mut ContainerTree<W>) {
        let properties = self.properties_mut(containers);
        if properties.name().is_some() {
            properties.lifetime = WorkspaceLifetime::Persistent;
        }
    }

    pub(super) fn is_empty_transient_numeric(
        &self,
        containers: &ContainerTree<W>,
        number: u32,
    ) -> bool {
        let properties = self.properties(containers);
        properties.numeric_number() == Some(number)
            && properties.lifetime == WorkspaceLifetime::Transient
            && !containers.workspace_has_windows(self.workspace)
    }

    pub(super) fn set_numeric_identity(
        &self,
        containers: &mut ContainerTree<W>,
        number: u32,
        lifetime: WorkspaceLifetime,
    ) {
        self.properties_mut(containers)
            .set_numeric_identity(number, lifetime);
    }

    pub(super) fn unname(&self, containers: &mut ContainerTree<W>) {
        self.properties_mut(containers).unname();
    }

    /// Something was just put on this workspace on `output`, which makes the workspace that
    /// output's own unless its identity already ties it to one.
    pub(super) fn claim_for_output(&self, containers: &mut ContainerTree<W>, output: &Output) {
        let properties = self.properties_mut(containers);
        if !properties.has_persistent_identity() {
            properties.original_output = OutputId::new(output);
        }
    }

    /// The workspace now belongs to `output`, whatever it belonged to before.
    pub(super) fn set_original_output(&self, containers: &mut ContainerTree<W>, output: &Output) {
        self.properties_mut(containers).original_output = OutputId::new(output);
    }

    pub(super) fn original_output<'a>(&self, containers: &'a ContainerTree<W>) -> &'a OutputId {
        &self.properties(containers).original_output
    }

    pub fn has_windows_or_persistent_identity(&self, containers: &ContainerTree<W>) -> bool {
        containers.workspace_has_windows(self.workspace) || self.has_persistent_identity(containers)
    }

    /// Whether the workspace holds something other than the output's sticky floats, as sway's
    /// `workspace_is_empty` counts it.
    ///
    /// Only for deciding whether a disconnected output's workspace is worth keeping, which is
    /// where sway asks it. Never for deciding a workspace may be dropped as it is: that would
    /// drop the sticky floats with it.
    pub fn has_non_sticky_windows_or_persistent_identity(
        &self,
        containers: &ContainerTree<W>,
    ) -> bool {
        containers.workspace_has_non_sticky_windows(self.workspace)
            || self.has_persistent_identity(containers)
    }

    /// Floating roots have just been moved off this workspace without going through it.
    pub(super) fn floats_left(&self, containers: &mut ContainerTree<W>) {
        self.floating_side()
            .cancel_resize_if_target_left(containers);
        self.update_focus_floating_tiling_after_removing(containers, true);
    }

    /// sway's `container_floating_move_to_center` for one of this workspace's floating roots.
    pub(super) fn center_floating_root(&self, containers: &mut ContainerTree<W>, root: NodeKey) {
        self.floating_side().center_root(containers, root);
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

        let properties = self.properties(containers);
        properties.name().is_none() || properties.lifetime == WorkspaceLifetime::Transient
    }

    pub fn scale(&self, containers: &ContainerTree<W>) -> smithay::output::Scale {
        self.properties(containers).output_scale
    }

    pub fn advance_animations(&self, containers: &mut ContainerTree<W>) {
        containers.advance_animations(self.workspace);
        self.runtime_mut(containers).advance_closing_animations();
    }

    pub fn are_animations_ongoing(&self, containers: &ContainerTree<W>) -> bool {
        containers.are_animations_ongoing(self.workspace)
            || self.runtime(containers).has_closing_windows()
    }

    pub fn are_transitions_ongoing(&self, containers: &ContainerTree<W>) -> bool {
        containers.are_transitions_ongoing(self.workspace)
            || self.runtime(containers).has_closing_windows()
    }

    pub fn update_render_elements(
        &self,
        containers: &mut ContainerTree<W>,
        is_active: bool,
        layer: RenderLayer,
    ) {
        self.runtime_mut(containers).is_active = is_active;
        let tiling_has_focus = self.layer_has_focus(containers, LayoutLayer::Tiled);
        let floating_has_focus = self.layer_has_focus(containers, LayoutLayer::Floating);
        containers.update_render_elements(self.workspace, tiling_has_focus);

        let view_rect = Rectangle::from_size(containers.view_size(self.workspace));
        self.floating_side().update_render_elements(
            containers,
            floating_has_focus,
            view_rect,
            layer,
        );

        if layer.is_normal() {
            let view_size = containers.view_size(self.workspace);
            let scale = containers.scale(self.workspace);
            self.runtime_mut(containers).shadow.update_render_elements(
                view_size,
                true,
                CornerRadius::default(),
                scale,
                1.,
            );
        }
    }

    pub fn update_config(&self, containers: &mut ContainerTree<W>, base_options: Rc<Options>) {
        let scale = containers.scale(self.workspace);
        let properties = self.properties(containers);
        let options = Rc::new(
            Options::clone(&base_options)
                .with_merged_layout(properties.layout_config.as_ref())
                .adjusted_for_scale(scale),
        );
        let output = containers.workspace_output(self.workspace).cloned();

        let view_size = containers.view_size(self.workspace);

        // Struts are the one layout option that changes the box everything is laid out in, so a
        // reload has to re-derive it rather than pass the box it was given on the last one.
        let working_area =
            if options.layout.struts != containers.options(self.workspace).layout.struts {
                match output.as_ref() {
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

        containers.update_config(self.workspace, view_size, working_area, options.clone());

        let shadow_config =
            compute_workspace_shadow_config(options.overview.workspace_shadow, view_size);
        self.runtime_mut(containers)
            .shadow
            .update_config(shadow_config);

        self.properties_mut(containers).base_options = base_options;
    }

    pub fn update_layout_config(
        &self,
        containers: &mut ContainerTree<W>,
        layout_config: Option<tiri_config::LayoutPart>,
    ) {
        let properties = self.properties_mut(containers);
        if properties.layout_config == layout_config {
            return;
        }

        properties.layout_config = layout_config;
        let base_options = properties.base_options.clone();
        self.update_config(containers, base_options);
    }

    pub fn update_shaders(&self, containers: &mut ContainerTree<W>) {
        containers.update_shaders(self.workspace);
        self.runtime_mut(containers).shadow.update_shaders();
    }

    fn is_floating_target(&self, containers: &ContainerTree<W>, window: Option<&W::Id>) -> bool {
        window.map_or(self.floating_is_active(containers), |id| {
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

    /// Route a focus command to the active layer.
    ///
    /// Centralizes the tiling-side `activate_tiling_content()` follow-up so it
    /// can never be forgotten by an individual focus method — the historical source of stale
    /// workspace-context bugs.
    fn dispatch_focus(
        &self,
        containers: &mut ContainerTree<W>,
        floating: impl FnOnce(FloatingRuntime<W>, &mut ContainerTree<W>) -> bool,
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
        &self,
        containers: &mut ContainerTree<W>,
        workspace: impl FnOnce(&mut ContainerTree<W>) -> bool,
        floating: impl FnOnce(FloatingRuntime<W>, &mut ContainerTree<W>) -> bool,
        tiled: impl FnOnce(&mut ContainerTree<W>) -> bool,
    ) -> bool {
        // The workspace's own arm descends into `ws->tiling`, the same side `tiled` acts on,
        // so both take the tiled epilogue. Only the floating arm leaves the layer alone.
        let (moved, landed_in_tiling) = match self.command_target(containers) {
            WorkspaceCommandTarget::Workspace => (workspace(containers), true),
            WorkspaceCommandTarget::Floating(_) => {
                (floating(self.floating_side(), containers), false)
            }
            WorkspaceCommandTarget::Tiled(_) => (tiled(containers), true),
        };
        if landed_in_tiling && moved {
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
        &self,
        containers: &mut ContainerTree<W>,
        direction: Direction,
        floating: impl FnOnce(FloatingRuntime<W>, &mut ContainerTree<W>),
        tiled: impl FnOnce(&mut ContainerTree<W>) -> bool,
    ) -> bool {
        match self.command_target(containers) {
            WorkspaceCommandTarget::Workspace => false,
            WorkspaceCommandTarget::Floating(target) => {
                if containers.is_floating_root(target) {
                    floating(self.floating_side(), containers);
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
        &self,
        containers: &mut ContainerTree<W>,
        floating: impl FnOnce(FloatingRuntime<W>, &mut ContainerTree<W>) -> R,
        tiled: impl FnOnce(&mut ContainerTree<W>) -> R,
    ) -> R {
        if self.floating_is_active(containers) {
            floating(self.floating_side(), containers)
        } else {
            tiled(containers)
        }
    }

    /// Route by the layer `window` lives in, defaulting to the active layer for `None`.
    fn dispatch_for_window<R>(
        &self,
        containers: &mut ContainerTree<W>,
        window: Option<&W::Id>,
        floating: impl FnOnce(FloatingRuntime<W>, &mut ContainerTree<W>) -> R,
        tiled: impl FnOnce(&mut ContainerTree<W>) -> R,
    ) -> R {
        if self.is_floating_target(containers, window) {
            floating(self.floating_side(), containers)
        } else {
            tiled(containers)
        }
    }

    /// Route one of Tiri's root-ordering commands. These commands address the tiled root list;
    /// directional `move container` commands use `dispatch_move_directional` on both layers.
    fn dispatch_tiling_root_reorder<R: Default>(
        &self,
        containers: &mut ContainerTree<W>,
        tiled: impl FnOnce(&mut ContainerTree<W>) -> R,
    ) -> R {
        match self.command_target(containers) {
            WorkspaceCommandTarget::Tiled(_) => tiled(containers),
            WorkspaceCommandTarget::Workspace | WorkspaceCommandTarget::Floating(_) => R::default(),
        }
    }

    fn focus_mode_toggle_targets_floating(&self, containers: &ContainerTree<W>) -> bool {
        !matches!(
            self.command_target(containers),
            WorkspaceCommandTarget::Floating(_)
        )
    }

    pub fn current_output<'a>(&self, containers: &'a ContainerTree<W>) -> Option<&'a Output> {
        containers.workspace_output(self.workspace)
    }

    pub fn active_window<'a>(&self, containers: &'a ContainerTree<W>) -> Option<&'a W> {
        if self.floating_is_active(containers) {
            containers.active_floating_window(self.workspace)
        } else {
            containers.focused_window_in_branch(self.workspace)
        }
    }

    pub fn active_window_mut<'a>(&self, containers: &'a mut ContainerTree<W>) -> Option<&'a mut W> {
        if self.floating_is_active(containers) {
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
            WorkspaceCommandTarget::Floating(key) => {
                containers.close_window_ids_in_branch(containers.branch_root(key))
            }
            WorkspaceCommandTarget::Tiled(_) => {
                containers.close_window_ids_in_branch(self.workspace)
            }
        };
        if !ids.is_empty() {
            return ids;
        }

        self.active_window(containers)
            .map(|window| vec![window.id().clone()])
            .unwrap_or_default()
    }

    /// Put this workspace, which is on no output, on `output` at `idx` of its workspaces.
    pub(super) fn attach(&self, containers: &mut ContainerTree<W>, output: OutputKey, idx: usize) {
        containers.attach_workspace(output, idx, self.workspace);
        let output = containers.workspace_output(self.workspace).unwrap().clone();

        let properties = self.properties_mut(containers);
        // Normalize original output: possibly replace connector with make/model/serial.
        if properties.original_output.matches(&output) {
            properties.original_output = OutputId::new(&output);
        }

        self.update_output_size(containers);

        for win in containers.windows_in_workspace(self.workspace) {
            Self::enter_output(containers, self.workspace, win);
        }
    }

    /// Take this workspace off the output it is on, if any.
    pub(super) fn detach(&self, containers: &mut ContainerTree<W>) {
        let Some(old) = containers.workspace_output(self.workspace).cloned() else {
            return;
        };
        containers.detach_workspace_from_its_output(self.workspace);
        for win in containers.windows_in_workspace(self.workspace) {
            win.output_leave(&old);
        }
    }

    /// Tell `window` it is on `workspace`'s output, with the output's scale and transform.
    fn enter_output(containers: &ContainerTree<W>, workspace: NodeKey, window: &W) {
        if let Some(output) = containers.workspace_output(workspace) {
            let properties = containers.workspace_properties(workspace);
            window.set_preferred_scale_transform(properties.output_scale, properties.transform);
            window.output_enter(output);
        }
    }

    fn leave_output(&self, containers: &ContainerTree<W>, window: &W) {
        if let Some(output) = containers.workspace_output(self.workspace) {
            window.output_leave(output);
        }
    }

    pub fn update_output_size(&self, containers: &mut ContainerTree<W>) {
        let output = containers.workspace_output(self.workspace).unwrap().clone();
        let output = &output;
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
        &self,
        containers: &mut ContainerTree<W>,
        scale: smithay::output::Scale,
        transform: Transform,
        size: Size<f64, Logical>,
        working_area: Rectangle<f64, Logical>,
    ) {
        let old = self.properties(containers);
        let scale_transform_changed = old.transform != transform
            || old.output_scale.integer_scale() != scale.integer_scale()
            || old.output_scale.fractional_scale() != scale.fractional_scale();
        let fractional_scale_changed =
            old.output_scale.fractional_scale() != scale.fractional_scale();
        if !scale_transform_changed
            && containers.view_size(self.workspace) == size
            && containers.working_area(self.workspace) == working_area
        {
            return;
        }

        let properties = self.properties_mut(containers);
        properties.output_scale = scale;
        properties.transform = transform;

        // The node owns the box, so this is the write that records the new one, and it comes
        // before the branch because both arms re-derive from what the node holds: the scale
        // arm through `update_config`, which reads the view size back out to re-inset it by
        // the struts.
        let options = containers.options(self.workspace).clone();
        containers.update_config(self.workspace, size, working_area, options);

        if fractional_scale_changed {
            // Options need to be recomputed for the new scale.
            let base_options = self.properties(containers).base_options.clone();
            self.update_config(containers, base_options);
        } else {
            let shadow_config = compute_workspace_shadow_config(
                containers.options(self.workspace).overview.workspace_shadow,
                size,
            );
            self.runtime_mut(containers)
                .shadow
                .update_config(shadow_config);
        }

        self.runtime_mut(containers).background.resize(size);

        if scale_transform_changed {
            for window in containers.windows_in_workspace(self.workspace) {
                window.set_preferred_scale_transform(scale, transform);
            }
        }
    }

    pub fn make_tile(&self, containers: &ContainerTree<W>, window: W) -> NewView<W> {
        NewView::new(Tile::new(
            window,
            containers.view_size(self.workspace),
            containers.scale(self.workspace),
            containers.clock().clone(),
            containers.options(self.workspace).clone(),
        ))
    }

    #[allow(clippy::too_many_arguments)]
    pub(super) fn add_tile(
        &self,
        containers: &mut ContainerTree<W>,
        mut tile: IncomingView<W>,
        target: WorkspaceAddWindowTarget<W>,
        activate: ActivateWindow,
        is_floating: bool,
    ) {
        Self::enter_output(
            containers,
            self.workspace,
            containers
                .incoming_tile(&tile)
                .expect("a workspace insertion must own a valid incoming view")
                .window(),
        );
        let floating_has_focus = self.floating_is_active(containers);
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
                        self.floating_side()
                            .add_tile_with_restore_hint(containers, tile, activate);
                    } else if grouped_floating {
                        self.floating_side()
                            .add_tile_to_active_container(containers, tile, activate);
                    } else {
                        self.floating_side().add_tile(containers, tile, activate);
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
                        .floating_side()
                        .container_allows_splits(containers, next_to);
                let wants_floating = is_floating || grouped_floating_target;

                let activate = activate.map_smart(|| {
                    self.active_window(containers)
                        .is_some_and(|win| win.id() == next_to)
                });

                if wants_floating {
                    if grouped_floating_target {
                        self.floating_side()
                            .add_tile_to_container_of(containers, next_to, tile, activate);
                    } else if floating_has_window {
                        self.floating_side()
                            .add_tile_above(containers, next_to, tile, activate);
                    } else {
                        self.center_new_incoming_floating_tile_on(containers, &mut tile, next_to);
                        self.floating_side().add_tile(containers, tile, activate);
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
            .floating_side()
            .clamp_within_working_area(working_area, pos, tile_size);
        tile.floating_pos = Some(self.floating_side().logical_to_size_frac(working_area, pos));
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

    fn activate_tiling_content(&self, containers: &mut ContainerTree<W>) {
        containers.clear_workspace_selection(self.workspace);
    }

    fn focus_tiling_key(
        &self,
        containers: &mut ContainerTree<W>,
        key: super::container::NodeKey,
    ) -> bool {
        let focused = containers.focus_inactive_tiling_key(key);
        if focused {
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
        &self,
        containers: &mut ContainerTree<W>,
        id: &W::Id,
        raise: bool,
    ) -> bool {
        if raise {
            self.floating_side().activate_window(containers, id)
        } else {
            self.floating_side()
                .activate_window_without_raising(containers, id)
        }
    }

    pub(super) fn restore_inactive_floating(&self, containers: &mut ContainerTree<W>) -> bool {
        let Some(id) = containers.inactive_floating_window_id(self.workspace) else {
            return false;
        };
        self.focus_floating_window(containers, &id, false)
    }

    pub(super) fn restore_inactive_tiling(
        &self,
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
        &self,
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
        &self,
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
        Self::enter_output(
            containers,
            self.workspace,
            containers
                .incoming_tile(&tile)
                .expect("a workspace insertion must own a valid incoming view")
                .window(),
        );

        containers.insert_tile_split(target, direction, tile, activate)?;

        if activate {
            self.activate_tiling_content(containers);
        }

        Ok(())
    }

    pub(super) fn add_tile_split_root(
        &self,
        containers: &mut ContainerTree<W>,
        direction: Direction,
        mut tile: IncomingView<W>,
        activate: bool,
    ) -> Result<(), IncomingView<W>> {
        containers
            .incoming_tile_mut(&mut tile)
            .expect("a workspace insertion must own a valid incoming view")
            .set_scratchpad(false);
        Self::enter_output(
            containers,
            self.workspace,
            containers
                .incoming_tile(&tile)
                .expect("a workspace insertion must own a valid incoming view")
                .window(),
        );

        containers.insert_tile_split_root(self.workspace, direction, tile, activate)?;

        if activate {
            self.activate_tiling_content(containers);
        }

        Ok(())
    }

    pub fn add_root_tiling_subtree(
        &self,
        containers: &mut ContainerTree<W>,
        subtree: RootTilingSubtree,
        activate: bool,
    ) {
        for tile in subtree.tiles(containers) {
            Self::enter_output(containers, self.workspace, tile.window());
        }

        containers.add_root_tiling_subtree(self.workspace, None, subtree, activate);

        if activate {
            self.activate_tiling_content(containers);
        }
    }

    fn update_focus_floating_tiling_after_removing(
        &self,
        containers: &mut ContainerTree<W>,
        removed_from_floating: bool,
    ) {
        // An elevation can only belong to the active layer (the inactive layer's elevation is
        // already dropped by construction), so clear it when that active layer empties out.
        if containers.tiled_is_empty(self.workspace) {
            containers.clear_selection_context();
        }

        // Which side has focus follows from what is left (`ContainerTree::focused_layer`);
        // only the seat's focus has to be put back on the tiled side when the floats are gone.
        if removed_from_floating && containers.floating_root_count(self.workspace) == 0 {
            self.activate_tiling_content(containers);
        }
    }

    pub(super) fn take_tile_for_transfer(
        &self,
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
            self.floating_side().take_tile_for_transfer(containers, id)
        } else {
            containers.take_tile_for_transfer(self.workspace, id, transaction)
        };

        self.leave_output(containers, transferred.tile(containers).window());

        self.update_focus_floating_tiling_after_removing(containers, from_floating);
        transferred
    }

    pub fn remove_active_root_tiling_subtree(
        &self,
        containers: &mut ContainerTree<W>,
    ) -> Option<RootTilingSubtree> {
        let from_floating = self.floating_is_active(containers);
        if from_floating {
            return None;
        }

        let subtree = containers.remove_active_root_tiling_subtree(self.workspace)?;

        if let Some(output) = containers.workspace_output(self.workspace) {
            for tile in subtree.tiles(containers) {
                tile.window().output_leave(output);
            }
        }

        self.update_focus_floating_tiling_after_removing(containers, from_floating);

        Some(subtree)
    }

    pub fn remove_active_tiling_subtree(
        &self,
        containers: &mut ContainerTree<W>,
    ) -> Option<RootTilingSubtree> {
        let from_floating = self.floating_is_active(containers);
        if from_floating {
            return None;
        }

        let subtree = containers.remove_active_tiling_subtree(self.workspace)?;

        if let Some(output) = containers.workspace_output(self.workspace) {
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
            self.floating_side()
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
        let properties = self.properties(containers);
        window.with_surfaces(|surface, data| {
            send_scale_transform(surface, data, properties.output_scale, properties.transform);
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

    /// Move the root container holding the focus among its siblings. Only the tiled side has
    /// root containers in that sense; a command aimed at a float does nothing.
    pub fn move_root_container(&self, containers: &mut ContainerTree<W>, position: RootPosition) {
        let workspace = self.workspace;
        self.dispatch_tiling_root_reorder(containers, |t| {
            t.move_root_container(workspace, position)
        });
    }

    /// Run a focus command on whichever side of the workspace the seat is on.
    pub(super) fn focus_command(
        &self,
        containers: &mut ContainerTree<W>,
        command: FocusCommand,
    ) -> bool {
        match command {
            FocusCommand::Direction(direction) => {
                self.focus_in_direction(containers, direction, true)
            }
            FocusCommand::Edge(direction) => {
                self.focus_edge(containers, direction);
                true
            }
            FocusCommand::DirectionOrEdge(direction) => {
                if !self.focus_in_direction(containers, direction, true) {
                    self.focus_edge(containers, direction.opposite());
                }
                true
            }
            FocusCommand::DirectionOr(primary, fallback) => {
                let _ = self.focus_in_direction(containers, primary, true)
                    || self.focus_in_direction(containers, fallback, true);
                true
            }
            FocusCommand::Parent => {
                self.focus_parent(containers);
                true
            }
            FocusCommand::Child => {
                self.focus_child(containers);
                true
            }
            FocusCommand::Sibling { forward, descend } => {
                self.focus_along_parent(containers, forward, descend)
            }
            FocusCommand::Floating => self.focus_mode(containers, true),
            FocusCommand::Tiling => self.focus_mode(containers, false),
            FocusCommand::ModeToggle => {
                let floating = self.focus_mode_toggle_targets_floating(containers);
                self.focus_mode(containers, floating)
            }
            FocusCommand::RootContainer(index) => {
                self.focus_root_container(containers, index);
                true
            }
            FocusCommand::WindowInRootContainer(index) => {
                self.focus_leaf_in_root_container(containers, index);
                true
            }
        }
    }

    /// sway's `focus floating|tiling`: the most recently focused window on that side.
    /// Returns whether the side holding focus changed.
    fn focus_mode(&self, containers: &mut ContainerTree<W>, floating: bool) -> bool {
        let was_floating_active = self.floating_is_active(containers);
        if !floating
            && was_floating_active
            && self
                .active_window(containers)
                .is_some_and(|window| window.is_pending_windowed_fullscreen())
        {
            // Match fullscreen focus constraints: don't move focus from an active fullscreen
            // floating container to tiling.
            return false;
        }

        if floating {
            if self.restore_inactive_floating(containers) {
                return true;
            }
            self.focus_floating(containers);
            return !was_floating_active && self.floating_is_active(containers);
        }

        if let Some(restored) = self.restore_inactive_tiling(containers) {
            return restored;
        }
        self.focus_tiling(containers);
        was_floating_active && !self.floating_is_active(containers)
    }

    /// sway's `focus <direction>`. Without `wrap`, running out of the workspace reports that
    /// nothing was focused, so the caller can go on to the next output.
    pub fn focus_in_direction(
        &self,
        containers: &mut ContainerTree<W>,
        direction: Direction,
        wrap: bool,
    ) -> bool {
        let workspace = self.workspace;
        self.dispatch_focus(
            containers,
            |f, tree| f.focus_in_direction(tree, direction, wrap),
            |t| t.focus_in_direction_scoped(workspace, direction, wrap),
        )
    }

    /// Focus the window furthest towards `direction`: niri's first and last column, top and
    /// bottom window.
    fn focus_edge(&self, containers: &mut ContainerTree<W>, direction: Direction) {
        let workspace = self.workspace;
        self.dispatch_focus(
            containers,
            |f, tree| {
                f.focus_edge(tree, direction);
                true
            },
            |t| {
                match direction {
                    Direction::Left => t.focus_root_container_first(workspace),
                    Direction::Right => t.focus_root_container_last(workspace),
                    Direction::Up => t.focus_top(workspace),
                    Direction::Down => t.focus_bottom(workspace),
                }
                true
            },
        );
    }

    fn focus_root_container(&self, containers: &mut ContainerTree<W>, index: usize) {
        if self.floating_is_active(containers) {
            self.focus_tiling(containers);
        }
        containers.focus_root_container(self.workspace, index);
        self.activate_tiling_content(containers);
    }

    fn focus_leaf_in_root_container(&self, containers: &mut ContainerTree<W>, index: u8) {
        if self.floating_is_active(containers) {
            return;
        }
        containers.focus_leaf_in_root_container(self.workspace, index);
        self.activate_tiling_content(containers);
    }

    pub(super) fn focus_entry_from_output_direction(
        &self,
        containers: &mut ContainerTree<W>,
        direction: Direction,
    ) -> bool {
        if containers.has_fullscreen_window(self.workspace) {
            // Fullscreen workspace targets resolve to the inactive focus under
            // the fullscreen subtree. Keep tiling active as-is.
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
                matches!(root_layout, LayoutMode::SplitH | LayoutMode::Tabbed)
            }
            Direction::Up | Direction::Down => {
                matches!(root_layout, LayoutMode::SplitV | LayoutMode::Stacked)
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
        self.activate_tiling_content(containers);
        true
    }

    pub(super) fn focus_workspace_node(&self, containers: &mut ContainerTree<W>) {
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
    fn activate_tiling_for_new_content(&self, containers: &mut ContainerTree<W>) {
        containers.clear_workspace_selection(self.workspace);
    }

    fn activate_floating_for_new_content(&self, containers: &mut ContainerTree<W>) {
        containers.clear_workspace_selection(self.workspace);
    }

    #[cfg(test)]
    pub(super) fn is_floating_workspace_context_active(
        &self,
        containers: &ContainerTree<W>,
    ) -> bool {
        self.floating_is_active(containers) && self.focus_is_elevated(containers)
    }

    /// Whether the workspace's real node is selected.
    #[cfg(test)]
    pub(super) fn tiling_targets_workspace(&self, containers: &ContainerTree<W>) -> bool {
        containers.workspace_is_selected(self.workspace)
    }

    #[cfg(test)]
    pub(super) fn is_tiling_workspace_context_active(&self, containers: &ContainerTree<W>) -> bool {
        !self.floating_is_active(containers) && self.tiling_targets_workspace(containers)
    }

    pub fn focus_window_by_id(&self, containers: &mut ContainerTree<W>, id: &W::Id) -> bool {
        if containers.window_is_floating_in(self.workspace, id) {
            containers.clear_selection();
            let _ = containers.focus_window_by_id(self.workspace, id);
            return true;
        }

        if containers.focus_window_by_id(self.workspace, id) {
            self.activate_tiling_content(containers);
            return true;
        }

        false
    }

    /// sway's `move <direction>` on whichever side the seat is on. Returns false when the tiled
    /// side has nowhere to move to, so the caller can go on to the next workspace or output.
    pub fn move_in_direction(
        &self,
        containers: &mut ContainerTree<W>,
        direction: Direction,
    ) -> bool {
        let workspace = self.workspace;
        self.dispatch_move_directional(
            containers,
            direction,
            |f, tree| f.move_in_direction(tree, direction),
            |t| t.move_command_target(workspace, direction),
        )
    }

    /// niri's `consume-or-expel-window-left|right`: move the window that way inside its
    /// branch, or, when it has nowhere to go there, split it vertically.
    pub fn consume_or_expel_window(
        &self,
        containers: &mut ContainerTree<W>,
        window: Option<&W::Id>,
        direction: Direction,
    ) {
        if let Some(id) = window {
            if !self.activate_window(containers, id) {
                return;
            }
        }
        let workspace = self.workspace;
        let split = LayoutCommand::Split(LayoutMode::SplitV);
        match self.command_target(containers) {
            WorkspaceCommandTarget::Floating(key) => {
                let branch = containers.branch_root(key);
                if !containers.move_in_branch(branch, direction) {
                    containers.apply_branch_layout_command(branch, split);
                }
            }
            WorkspaceCommandTarget::Workspace | WorkspaceCommandTarget::Tiled(_) => {
                if !containers.move_command_target(workspace, direction) {
                    containers.apply_branch_layout_command(workspace, split);
                }
                self.activate_tiling_content(containers);
            }
        }
    }

    /// sway's `move position center`, which is a floating-layer command.
    ///
    /// A tiled window has no position to set — it fills the slot its container gives it — so
    /// the tiling half is inert rather than inventing a meaning for centering inside a tree.
    /// sway says the same thing by refusing `move position` on a tiled container.
    pub fn center_window(&self, containers: &mut ContainerTree<W>, id: Option<&W::Id>) {
        if self.is_floating_target(containers, id) {
            self.floating_side().center_window(containers, id);
        }
    }

    /// Run a size command on `window`, or on what this workspace's commands are aimed at.
    pub fn size_command(
        &self,
        containers: &mut ContainerTree<W>,
        window: Option<&W::Id>,
        command: SizeCommand,
    ) {
        match command {
            SizeCommand::Resize(request) => self.resize_window(containers, window, request),
            SizeCommand::CyclePreset {
                axis: ResizeAxis::Horizontal,
                forwards,
            } => self.toggle_window_width(containers, window, forwards),
            SizeCommand::CyclePreset {
                axis: ResizeAxis::Vertical,
                forwards,
            } => self.toggle_window_height(containers, window, forwards),
            SizeCommand::ResetHeight => self.reset_window_height(containers, window),
            SizeCommand::CycleRootPreset { forwards } => self.toggle_width(containers, forwards),
            SizeCommand::SetRootWidth(change) => self.set_root_container_width(containers, change),
            SizeCommand::ExpandRootToAvailableWidth => {
                self.expand_root_container_to_available_width(containers)
            }
        }
    }

    fn toggle_width(&self, containers: &mut ContainerTree<W>, forwards: bool) {
        let workspace = self.workspace;
        self.dispatch_active_layer(
            containers,
            |f, tree| f.toggle_window_width(tree, None, forwards),
            |t| t.toggle_width(workspace, forwards),
        );
    }

    fn set_root_container_width(&self, containers: &mut ContainerTree<W>, change: SizeChange) {
        let workspace = self.workspace;
        self.dispatch_active_layer(
            containers,
            |f, tree| f.set_window_width(tree, None, change, true),
            |t| t.set_root_container_width(workspace, change),
        );
    }

    /// Route one semantic resize request by the addressed node, not merely by layer.
    fn resize_window(
        &self,
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
            self.floating_side()
                .resize_window(containers, window, request);
        } else {
            // A descendant of a floating root is tiled inside that root. sway takes the
            // geometric floating path only for the root itself.
            containers.resize_node(target, request);
        }
    }

    fn reset_window_height(&self, containers: &mut ContainerTree<W>, window: Option<&W::Id>) {
        let workspace = self.workspace;
        self.dispatch_for_window(
            containers,
            window,
            |_, _| {},
            |t| t.reset_window_height(workspace, window),
        );
    }

    fn toggle_window_width(
        &self,
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

    fn toggle_window_height(
        &self,
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

    fn expand_root_container_to_available_width(&self, containers: &mut ContainerTree<W>) {
        let workspace = self.workspace;
        self.dispatch_active_layer(
            containers,
            |_, _| {},
            |t| t.expand_root_container_to_available_width(workspace),
        );
    }

    /// sway's `focus next|prev [sibling]` inside whichever tree branch owns the selection.
    fn focus_along_parent(
        &self,
        containers: &mut ContainerTree<W>,
        forward: bool,
        descend: bool,
    ) -> bool {
        match self.command_target(containers) {
            WorkspaceCommandTarget::Workspace => false,
            WorkspaceCommandTarget::Floating(_) => self
                .floating_side()
                .focus_along_parent(containers, forward, descend),
            WorkspaceCommandTarget::Tiled(_) => {
                containers.focus_along_parent(self.workspace, forward, descend)
            }
        }
    }

    fn focus_parent(&self, containers: &mut ContainerTree<W>) {
        match self.command_target(containers) {
            WorkspaceCommandTarget::Workspace => {}
            WorkspaceCommandTarget::Floating(_) => {
                let _ = self.floating_side().focus_parent(containers);
            }
            WorkspaceCommandTarget::Tiled(_) => {
                let _ = containers.focus_parent(self.workspace);
            }
        }
    }

    fn focus_child(&self, containers: &mut ContainerTree<W>) {
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
        &self,
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
            self.floating_side()
                .set_fullscreen(containers, window, is_fullscreen);
            return;
        }

        containers.set_fullscreen(self.workspace, window, is_fullscreen);
    }

    pub fn toggle_fullscreen(&self, containers: &mut ContainerTree<W>, window: &W::Id) {
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
        &self,
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

    pub fn toggle_window_floating(&self, containers: &mut ContainerTree<W>, id: Option<&W::Id>) {
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
        &self,
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
            && containers.selected_container_in(self.workspace)
        {
            return Some(FloatingTransfer::Float(FloatTransfer::SelectedContainer));
        }

        if is_floating {
            // `container_set_floating` asks `seat_get_focus_inactive_tiling` where the same
            // node lands (`sway/tree/container.c:1039-1057`). Its old parent is deliberately
            // not a restore target; sway removed that parent link when the node became floating.
            if !explicit_window {
                let was_workspace = self
                    .floating_side()
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

        Some(FloatingTransfer::Float(FloatTransfer::Window { id }))
    }

    /// Execute a resolved tiling-to-floating transfer.
    ///
    /// The return value says whether the moved window should animate from its old render
    /// position. Whole-container transfers intentionally do not run that animation.
    fn execute_float_transfer(
        &self,
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
                    .floating_side()
                    .add_subtree(containers, subtree, rect, true)
                {
                    return false;
                }
                false
            }
            FloatTransfer::SelectedContainer => {
                let Some((subtree, rect)) = containers.take_selected_subtree(self.workspace) else {
                    return false;
                };
                if !self
                    .floating_side()
                    .add_subtree(containers, subtree, rect, false)
                {
                    return false;
                }
                false
            }
            FloatTransfer::Window { id } => {
                let Some((subtree, rect)) =
                    containers.subtree_for_window_floating(self.workspace, &id)
                else {
                    return false;
                };
                if let Some(tile) = containers.get_tile_mut(subtree) {
                    tile.stop_move_animations();
                }

                if !self
                    .floating_side()
                    .add_subtree(containers, subtree, rect, false)
                {
                    return false;
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
        &self,
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
                if self.floating_side().unfloat_container(
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

                let unfloated =
                    self.floating_side()
                        .unfloat_window(containers, &id, tiling_reference.as_ref());
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
                let unfloated =
                    self.floating_side()
                        .unfloat_window(containers, &id, tiling_reference.as_ref());
                if unfloated && target_is_active {
                    self.finish_active_unfloat(containers);
                }
                unfloated
            }
        }
    }

    fn finish_active_unfloat(&self, containers: &mut ContainerTree<W>) {
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
        &self,
        containers: &mut ContainerTree<W>,
        id: &W::Id,
    ) -> Option<ViewTransfer> {
        self.set_fullscreen(containers, id, false);
        self.take_tile_for_scratchpad(containers, id)
    }

    /// `container_fullscreen_disable` on whatever this workspace's one fullscreen pointer
    /// names, leaf or container.
    pub fn clear_fullscreen(&self, containers: &mut ContainerTree<W>) {
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
        &self,
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
            transferred.tile_mut(containers).floating_pos =
                Some(self.floating_side().logical_to_size_frac(working_area, pos));

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
        &self,
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
        &self,
        containers: &mut ContainerTree<W>,
        mut tile: IncomingView<W>,
        activate: bool,
    ) {
        let tile_ref = containers
            .incoming_tile_mut(&mut tile)
            .expect("a scratchpad insertion must own a valid incoming view");
        tile_ref.set_scratchpad(true);
        tile_ref.window_mut().set_floating(true);
        Self::enter_output(
            containers,
            self.workspace,
            containers
                .incoming_tile(&tile)
                .expect("a scratchpad insertion must own a valid incoming view")
                .window(),
        );
        self.floating_side().add_tile(containers, tile, activate);

        if activate || containers.tiled_is_empty(self.workspace) {}
    }

    pub fn set_window_floating(
        &self,
        containers: &mut ContainerTree<W>,
        id: Option<&W::Id>,
        floating: bool,
    ) {
        if self.is_floating_target(containers, id) == floating {
            return;
        }

        self.toggle_window_floating(containers, id);
    }

    fn focus_floating(&self, containers: &mut ContainerTree<W>) {
        if !self.floating_is_active(containers) {
            self.switch_focus_floating_tiling(containers);
        }
    }

    pub(super) fn focus_tiling(&self, containers: &mut ContainerTree<W>) {
        if self.floating_is_active(containers) {
            self.switch_focus_floating_tiling(containers);
        }
    }

    fn switch_focus_floating_tiling(&self, containers: &mut ContainerTree<W>) {
        if containers.floating_root_count(self.workspace) == 0
            || containers.tiled_is_empty(self.workspace)
        {
            return;
        }

        containers.clear_selection_context();
        // sway's `focus mode_toggle` focuses the other side's inactive focus; which side holds
        // focus is then just what the seat says.
        if self.floating_is_active(containers) {
            self.activate_tiling_content(containers);
        } else if let Some(id) = containers.inactive_floating_window_id(self.workspace) {
            self.floating_side().activate_window(containers, &id);
        }
    }

    pub fn clear_selection_context(&self, containers: &mut ContainerTree<W>) {
        containers.clear_selection_context();
    }

    pub fn move_floating_window(
        &self,
        containers: &mut ContainerTree<W>,
        id: Option<&W::Id>,
        x: PositionChange,
        y: PositionChange,
        animate: bool,
    ) {
        if self.is_floating_target(containers, id) {
            self.floating_side()
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
                .floating_side()
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

            let pos = self.floating_side().logical_to_size_frac(working_area, pos);
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
        &self,
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
        if self.floating_is_active(containers) {
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
        let tiling_focus_ring = focus_ring && !self.floating_is_active(containers);
        containers.render(
            self.workspace,
            ctx,
            xray_pos,
            BranchRenderParams {
                has_focus: self.layer_has_focus(containers, LayoutLayer::Tiled),
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
        let tiling_focus_ring = focus_ring && !self.floating_is_active(containers);
        if let Some(elem) = containers.render_as_offscreen(
            self.workspace,
            renderer,
            target,
            BranchRenderParams {
                has_focus: self.layer_has_focus(containers, LayoutLayer::Tiled),
                focus_ring: tiling_focus_ring,
                layer: RenderLayer::Normal,
            },
        ) {
            push(elem.into());
        }
    }

    #[allow(clippy::too_many_arguments)]
    pub(super) fn render_floating<R: NiriRenderer>(
        &self,
        containers: &ContainerTree<W>,
        ctx: RenderCtx<R>,
        xray_pos: XrayPos,
        focus_ring: bool,
        layer: RenderLayer,
        roots: FloatingRoots,
        push: &mut dyn FnMut(WorkspaceRenderElement<R>),
    ) {
        if !self.is_floating_visible(containers) && layer.is_normal() {
            return;
        }

        let view_rect = Rectangle::from_size(containers.view_size(self.workspace));
        let floating_focus_ring = focus_ring && self.floating_is_active(containers);
        self.floating_side().render(
            containers,
            ctx,
            xray_pos,
            view_rect,
            BranchRenderParams {
                has_focus: self.layer_has_focus(containers, LayoutLayer::Floating),
                focus_ring: floating_focus_ring,
                layer,
            },
            roots,
            &mut |elem| push(elem.into()),
        );
    }

    pub fn render_shadow<R: NiriRenderer>(
        &self,
        containers: &ContainerTree<W>,
        renderer: &mut R,
        push: &mut dyn FnMut(ShadowRenderElement),
    ) {
        self.runtime(containers)
            .shadow
            .render(renderer, Point::from((0., 0.)), push);
    }

    pub fn render_background(&self, containers: &ContainerTree<W>) -> SolidColorRenderElement {
        SolidColorRenderElement::from_buffer(
            &self.runtime(containers).background,
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
        self.floating_is_active(containers)
            || self.floating_raised(containers)
            || containers.floating_fullscreen_key(self.workspace).is_some()
            || !self.render_above_top_layer(containers)
    }

    pub fn store_unmap_snapshot_if_empty(
        &self,
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

    pub fn clear_unmap_snapshot(&self, containers: &mut ContainerTree<W>, window: &W::Id) {
        for tile in containers.tiles_mut_in_workspace(self.workspace) {
            if tile.window().id() == window {
                let _ = tile.take_unmap_snapshot();
                return;
            }
        }
    }

    pub fn start_close_animation_for_window(
        &self,
        containers: &mut ContainerTree<W>,
        renderer: &mut GlesRenderer,
        window: &W::Id,
        blocker: TransactionBlocker,
    ) {
        if containers.window_is_floating_in(self.workspace, window) {
            self.floating_side()
                .start_close_animation_for_window(containers, renderer, window, blocker);
        } else {
            containers.start_close_animation_for_window(self.workspace, renderer, window, blocker);
        }
    }

    pub fn start_close_animation_for_tile(
        &self,
        containers: &mut ContainerTree<W>,
        renderer: &mut GlesRenderer,
        snapshot: TileRenderSnapshot,
        tile_size: Size<f64, Logical>,
        tile_pos: Point<f64, Logical>,
        blocker: TransactionBlocker,
    ) {
        self.floating_side().start_close_animation_for_tile(
            containers, renderer, snapshot, tile_size, tile_pos, blocker,
        );
    }

    pub fn start_open_animation(&self, containers: &mut ContainerTree<W>, id: &W::Id) -> bool {
        containers.start_open_animation(self.workspace, id)
            || self.floating_side().start_open_animation(containers, id)
    }

    pub fn window_under<'a>(
        &self,
        containers: &'a ContainerTree<W>,
        pos: Point<f64, Logical>,
    ) -> Option<(&'a W, HitType)> {
        if self.is_floating_visible(containers) {
            if let Some(rv) = self.floating_side().window_under(containers, pos) {
                return Some(rv);
            }
        }

        containers.window_under(self.workspace, pos)
    }

    pub fn resize_hit_under(
        &self,
        containers: &ContainerTree<W>,
        pos: Point<f64, Logical>,
    ) -> Option<ResizeHit<W::Id>> {
        if containers.is_active_pending_fullscreen(self.workspace) {
            return None;
        }

        if self.is_floating_visible(containers) {
            match self.floating_side().resize_hit_under(containers, pos) {
                FloatingResizeResult::Hit(hit) => {
                    let cursor = if !hit.external_edges.is_empty() {
                        hit.external_edges.outward_cursor_icon()
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
            if self.floating_is_active(containers) {
                return None;
            }
        }

        containers.resize_hit_under(self.workspace, pos)
    }

    pub fn descendants_added(&self, containers: &mut ContainerTree<W>, id: &W::Id) -> bool {
        self.floating_side().descendants_added(containers, id)
    }

    pub fn update_window(
        &self,
        containers: &mut ContainerTree<W>,
        window: &W::Id,
        serial: Option<Serial>,
    ) {
        if !self
            .floating_side()
            .update_window(containers, window, serial)
        {
            containers.update_window(self.workspace, window, serial);
        }
    }

    pub fn refresh(&self, containers: &mut ContainerTree<W>, is_active: bool, is_focused: bool) {
        let _span = tracy_client::span!("Workspace::refresh");
        containers.refresh(
            self.workspace,
            is_active && !self.floating_is_active(containers),
            is_focused,
        );
        self.floating_side().refresh(
            containers,
            is_active && self.floating_is_active(containers),
            is_focused,
        );
    }

    pub fn is_urgent(&self, containers: &ContainerTree<W>) -> bool {
        containers
            .windows_in_workspace(self.workspace)
            .any(|win| win.is_urgent())
    }

    pub fn activate_window(&self, containers: &mut ContainerTree<W>, window: &W::Id) -> bool {
        if self.floating_side().activate_window(containers, window) {
            true
        } else if containers.focus_window_by_id(self.workspace, window) {
            self.activate_tiling_content(containers);
            true
        } else {
            false
        }
    }

    pub fn activate_window_without_raising(
        &self,
        containers: &mut ContainerTree<W>,
        window: &W::Id,
    ) -> bool {
        if self
            .floating_side()
            .activate_window_without_raising(containers, window)
        {
            true
        } else {
            let floating_had_focus = self.floating_is_active(containers);
            if !containers.focus_window_by_id(self.workspace, window) {
                return false;
            }
            self.activate_tiling_content(containers);
            if floating_had_focus {
                let generation = containers.seat_focus_generation();
                self.runtime_mut(containers).floating_raised_at = Some(generation);
            }
            true
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
        &self,
        containers: &mut ContainerTree<W>,
        window: W::Id,
        edges: ResizeEdge,
    ) -> bool {
        if containers.window_is_floating_in(self.workspace, &window) {
            self.floating_side()
                .interactive_resize_begin(containers, window, edges)
        } else {
            containers.interactive_resize_begin(self.workspace, window, edges)
        }
    }

    pub fn interactive_resize_begin_at(
        &self,
        containers: &mut ContainerTree<W>,
        window: W::Id,
        edges: ResizeEdge,
        pos: Point<f64, Logical>,
    ) -> bool {
        if containers.window_is_floating_in(self.workspace, &window) {
            self.floating_side()
                .interactive_resize_begin(containers, window, edges)
        } else {
            containers.interactive_resize_begin_at(self.workspace, window, edges, pos)
        }
    }

    pub fn interactive_resize_update(
        &self,
        containers: &mut ContainerTree<W>,
        window: &W::Id,
        delta: Point<f64, Logical>,
    ) -> bool {
        if containers.window_is_floating_in(self.workspace, window) {
            self.floating_side()
                .interactive_resize_update(containers, window, delta)
        } else {
            containers.interactive_resize_update(self.workspace, window, delta)
        }
    }

    pub fn interactive_resize_end(
        &self,
        containers: &mut ContainerTree<W>,
        window: Option<&W::Id>,
    ) {
        if let Some(window) = window {
            if containers.window_is_floating_in(self.workspace, window) {
                self.floating_side()
                    .interactive_resize_end(containers, Some(window));
            } else {
                containers.interactive_resize_end(self.workspace, Some(window));
            }
        } else {
            self.floating_side()
                .interactive_resize_end(containers, None);
            containers.interactive_resize_end(self.workspace, None);
        }
    }

    /// Whether the seat was last on this workspace's floating side.
    pub fn floating_is_active(&self, containers: &ContainerTree<W>) -> bool {
        containers.focused_layer(self.workspace) == LayoutLayer::Floating
    }

    /// Whether `layer` holds the focus of this workspace while the workspace holds the
    /// compositor's.
    pub(super) fn layer_has_focus(
        &self,
        containers: &ContainerTree<W>,
        layer: LayoutLayer,
    ) -> bool {
        self.runtime(containers).is_active && containers.focused_layer(self.workspace) == layer
    }

    /// Whether the floats stay drawn above a tiled fullscreen although focus went to the tiled
    /// side: true until focus next changes after a focus-follows-mouse move.
    fn floating_raised(&self, containers: &ContainerTree<W>) -> bool {
        self.runtime(containers).floating_raised_at == Some(containers.seat_focus_generation())
    }

    pub fn floating_logical_to_size_frac(
        &self,
        containers: &ContainerTree<W>,
        logical_pos: Point<f64, Logical>,
    ) -> Point<f64, SizeFrac> {
        self.floating_side()
            .logical_to_size_frac(containers.working_area(self.workspace), logical_pos)
    }

    pub(super) fn floating_container_allows_splits(
        &self,
        containers: &ContainerTree<W>,
        id: &W::Id,
    ) -> bool {
        self.floating_side().container_allows_splits(containers, id)
    }

    pub(super) fn floating_container_pos(
        &self,
        containers: &ContainerTree<W>,
        id: &W::Id,
    ) -> Option<Point<f64, Logical>> {
        self.floating_side().container_pos(containers, id)
    }

    pub(super) fn move_floating_container_for_window_to(
        &self,
        containers: &mut ContainerTree<W>,
        id: &W::Id,
        pos: Point<f64, Logical>,
    ) -> bool {
        self.floating_side()
            .move_container_for_window_to(containers, id, pos, false)
    }

    pub fn layout_config<'a>(
        &self,
        containers: &'a ContainerTree<W>,
    ) -> Option<&'a tiri_config::LayoutPart> {
        self.properties(containers).layout_config.as_ref()
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
    pub fn floating<'a>(&self, containers: &'a ContainerTree<W>) -> FloatingTestView<'a, W> {
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
    pub fn debug_workspace_layout(&self, containers: &ContainerTree<W>) -> LayoutMode {
        containers.debug_workspace_layout(self.workspace)
    }

    pub fn verify_invariants(&self, containers: &ContainerTree<W>, move_win_id: Option<&W::Id>) {
        use approx::assert_abs_diff_eq;

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

        let scale = containers.scale(self.workspace);
        assert!(scale > 0.);
        assert!(scale.is_finite());

        let properties = self.properties(containers);
        let options = Options::clone(&properties.base_options)
            .with_merged_layout(properties.layout_config.as_ref())
            .adjusted_for_scale(scale);
        assert_eq!(
            &**containers.options(self.workspace),
            &options,
            "options must be base options adjusted for scale"
        );

        assert!(containers.view_size(self.workspace).w > 0.);
        assert!(containers.view_size(self.workspace).h > 0.);

        assert_eq!(
            self.runtime(containers).background.size(),
            containers.view_size(self.workspace)
        );
        assert_eq!(
            self.runtime(containers).background.color().components(),
            options.layout.background_color.to_array_unpremul(),
        );

        assert_eq!(
            containers.working_area(self.workspace),
            containers.parent_area(self.workspace)
        );
        containers.verify_invariants(self.workspace);

        self.floating_side().verify_runtime_invariants(containers);

        if containers.floating_root_count(self.workspace) == 0 {
            assert!(
                !self.floating_is_active(containers),
                "when floating is empty it must never be active"
            );
        } else if containers.tiled_is_empty(self.workspace) {
            assert!(
                self.floating_is_active(containers),
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
        if self.floating_is_active(containers) {
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
