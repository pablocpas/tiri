use std::cmp::max;
use std::rc::Rc;
use std::time::Duration;

use smithay::backend::renderer::element::Kind;
use smithay::backend::renderer::gles::GlesRenderer;
use smithay::desktop::{layer_map_for_output, Window};
use smithay::input::pointer::CursorIcon;
use smithay::output::Output;
use smithay::reexports::wayland_protocols::xdg::shell::server::xdg_toplevel;
use smithay::reexports::wayland_server::protocol::wl_surface::WlSurface;
use smithay::utils::{Logical, Point, Rectangle, Serial, Size, Transform};
use smithay::wayland::compositor::with_states;
use smithay::wayland::shell::xdg::SurfaceCachedState;
use tiri_config::utils::MergeWith as _;
use tiri_config::{CornerRadius, OutputName, PresetSize, Workspace as WorkspaceConfig};
use tiri_ipc::{ColumnDisplay, LayoutTreeNode, PositionChange, SizeChange, WindowLayout};

use super::container::{Direction, InactiveTilingReference, InsertParentInfo, Layout, NodeKey};
use super::floating::{
    compute_toplevel_bounds, FloatingResizeResult, FloatingSpace, FloatingSpaceRenderElement,
};
use super::legacy_column::{Column, ColumnWidth};
use super::shadow::Shadow;
use super::tile::{Tile, TileRenderSnapshot};
use super::tree_space::{RootTilingSubtree, TreeSpace, TreeSpaceRenderElement};
use super::{
    ActivateWindow, HitType, InsertPosition, InteractiveResizeData, LayoutCycleEntry,
    LayoutElement, Options, RemovedTile, ResizeHit, ResizeRequest, SizeFrac,
};
use crate::animation::Clock;
use crate::niri_render_elements;
use crate::render_helpers::offscreen::OffscreenRenderElement;
use crate::render_helpers::renderer::NiriRenderer;
use crate::render_helpers::shadow::ShadowRenderElement;
use crate::render_helpers::solid_color::{SolidColorBuffer, SolidColorRenderElement};
use crate::render_helpers::xray::{Xray, XrayPos};
use crate::render_helpers::{RenderCtx, RenderTarget};
use crate::utils::id::IdCounter;
use crate::utils::transaction::{Transaction, TransactionBlocker};
use crate::utils::{
    center_preferring_top_left_in_area, ensure_min_max_size, ensure_min_max_size_maybe_zero,
    output_size, send_scale_transform, ResizeEdge,
};
use crate::window::ResolvedWindowRules;

#[derive(Debug)]
pub struct Workspace<W: LayoutElement> {
    /// The workspace's layout: the arena both sides live in, and the tiled side's own state.
    ///
    /// sway's workspace holds `tiling` and `floating` as two lists over one set of
    /// containers. This is that set, and the floating side asks it for the arena rather than
    /// keeping one — not because the tiled side owns it, but because this is the workspace.
    space: TreeSpace<W>,

    /// What the floating side keeps that the tiled side has no use for: where each group
    /// sits, and the order they stack in.
    floating: FloatingSpace<W>,

    /// Whether the floating layout is active instead of the tiling layout.
    floating_is_active: FloatingActive,

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

    /// Latest known view size for this workspace.
    ///
    /// This should be computed from the current workspace output size, or, if all outputs have
    /// been disconnected, preserved until a new output is connected.
    view_size: Size<f64, Logical>,

    /// Latest known working area for this workspace.
    ///
    /// Not rounded to physical pixels.
    ///
    /// This is similar to view size, but takes into account things like layer shell exclusive
    /// zones.
    working_area: Rectangle<f64, Logical>,

    /// This workspace's shadow in the overview.
    shadow: Shadow,

    /// This workspace's background.
    background_buffer: SolidColorBuffer,

    /// Clock for driving animations.
    pub(super) clock: Clock,

    /// Configurable properties of the layout as received from the parent monitor.
    pub(super) base_options: Rc<Options>,

    /// Configurable properties of the layout with logical sizes adjusted for the current `scale`.
    pub(super) options: Rc<Options>,

    /// Stable identity of this workspace.
    identity: WorkspaceIdentity,
    /// Whether the workspace should survive when empty and inactive.
    lifetime: WorkspaceLifetime,

    /// Layout config overrides for this workspace.
    layout_config: Option<tiri_config::LayoutPart>,

    /// Unique ID of this workspace.
    id: WorkspaceId,
}

#[cfg(test)]
pub struct FloatingTestView<'a, W: LayoutElement> {
    floating: &'a FloatingSpace<W>,
    space: &'a TreeSpace<W>,
}

#[cfg(test)]
impl<'a, W: LayoutElement> FloatingTestView<'a, W> {
    pub fn tiles(&self) -> impl Iterator<Item = &'a Tile<W>> + 'a {
        self.floating.tiles(self.space)
    }

    pub fn root_layout_for_window(&self, id: &W::Id) -> Option<Layout> {
        self.floating.root_layout_for_window(self.space, id)
    }

    pub fn selected_is_container(&self, id: Option<&W::Id>) -> bool {
        self.floating.selected_is_container(self.space, id)
    }

    pub fn wrapper_selected_for_window(&self, id: &W::Id) -> bool {
        self.floating.wrapper_selected_for_window(self.space, id)
    }

    pub fn is_fullscreen(&self, id: &W::Id) -> bool {
        self.floating.is_fullscreen(self.space, id)
    }

    pub fn active_window(&self) -> Option<&'a W> {
        self.floating.active_window(self.space)
    }

    pub fn debug_tree_for_window(&self, id: &W::Id) -> Option<String>
    where
        W::Id: std::fmt::Display,
    {
        self.floating.debug_tree_for_window(self.space, id)
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) enum WorkspaceIdentity {
    Anonymous,
    Numeric { number: u32, name: String },
    Named(String),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum WorkspaceLifetime {
    Persistent,
    Transient,
}

#[derive(Debug, Clone)]
pub struct OutputId(String);

impl OutputId {
    pub fn matches(&self, output: &Output) -> bool {
        let output_name = output.user_data().get::<OutputName>().unwrap();
        output_name.matches(&self.0)
    }
}

static WORKSPACE_ID_COUNTER: IdCounter = IdCounter::new();

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct WorkspaceId(u64);

impl WorkspaceId {
    fn next() -> WorkspaceId {
        WorkspaceId(WORKSPACE_ID_COUNTER.next())
    }

    pub fn get(self) -> u64 {
        self.0
    }

    pub fn specific(id: u64) -> Self {
        Self(id)
    }
}

niri_render_elements! {
    WorkspaceRenderElement<R> => {
        Tiling = TreeSpaceRenderElement<R>,
        Floating = FloatingSpaceRenderElement<R>,
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

/// How many tiling restore targets a workspace keeps.
///
/// The stack is never pruned when the tree changes under it, so this bound is the only thing
/// standing between it and one entry per focus change for the life of the workspace.
/// Whether the floating space is active.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum FloatingActive {
    /// The tiling space is active.
    No,
    /// The tiling space is active, but the floating space should render on top, even if the
    /// active tiling window is fullscreen.
    ///
    /// This is necessary for focus-follows-mouse that activates but doesn't raise the window to
    /// avoid being annoying.
    NoButRaised,
    /// The floating space is active.
    Yes,
}

/// A resolved tiling-to-floating mutation.
///
/// Resolution names the semantic thing sway moves. Execution is the only layer that touches
/// the shared container arena, floating stack, or focus caches.
enum FloatTransfer<I> {
    Workspace { focus_id: I },
    SelectedContainer { focus_id: I },
    Window { id: I, target_is_active: bool },
}

/// A resolved floating-to-tiling mutation.
enum UnfloatTransfer<I> {
    Container {
        id: I,
        target_is_active: bool,
        tiling_reference: Option<InactiveTilingReference>,
        was_workspace: bool,
        tiling_was_empty: bool,
    },
    Window {
        id: I,
        target_is_active: bool,
        tiling_reference: Option<InactiveTilingReference>,
    },
}

enum FloatingTransfer<I> {
    Float(FloatTransfer<I>),
    Unfloat(UnfloatTransfer<I>),
}

impl<I> FloatingTransfer<I> {
    fn window_id(&self) -> &I {
        match self {
            Self::Float(FloatTransfer::Workspace { focus_id })
            | Self::Float(FloatTransfer::SelectedContainer { focus_id, .. }) => focus_id,
            Self::Float(FloatTransfer::Window { id, .. })
            | Self::Unfloat(UnfloatTransfer::Container { id, .. })
            | Self::Unfloat(UnfloatTransfer::Window { id, .. }) => id,
        }
    }

    fn may_animate_window_position(&self) -> bool {
        matches!(
            self,
            Self::Float(FloatTransfer::Window { .. })
                | Self::Unfloat(UnfloatTransfer::Container { .. })
                | Self::Unfloat(UnfloatTransfer::Window { .. })
        )
    }
}

/// Where to put a newly added window.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub enum WorkspaceAddWindowTarget<'a, W: LayoutElement> {
    /// No particular preference.
    #[default]
    Auto,
    /// As a new column at this index.
    NewColumnAt(usize),
    /// Next to this existing window.
    NextTo(&'a W::Id),
}

impl OutputId {
    pub fn new(output: &Output) -> Self {
        let output_name = output.user_data().get::<OutputName>().unwrap();
        Self(output_name.format_make_model_serial_or_connector())
    }
}

impl FloatingActive {
    fn get(self) -> bool {
        self == Self::Yes
    }
}

/// Tell a tile which side it will restore to.
///
/// Landing in tiling is also what ends a tile's stay in the scratchpad, so the two travel
/// together — every `add_tile` arm answers the same question and has to answer it once.
fn mark_restore_to_floating<W: LayoutElement>(tile: &mut Tile<W>, wants_floating: bool) {
    if !wants_floating {
        tile.set_scratchpad(false);
    }
    tile.restore_to_floating = wants_floating;
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

    pub fn new(output: Output, clock: Clock, options: Rc<Options>) -> Self {
        Self::new_with_config(output, None, clock, options)
    }

    pub fn new_with_config(
        output: Output,
        mut config: Option<WorkspaceConfig>,
        clock: Clock,
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
        let working_area = compute_working_area(&output);

        let space = TreeSpace::new(
            view_size,
            working_area,
            scale.fractional_scale(),
            clock.clone(),
            options.clone(),
        );

        let floating = FloatingSpace::new();

        let shadow_config =
            compute_workspace_shadow_config(options.overview.workspace_shadow, view_size);

        Self {
            space,
            floating,
            floating_is_active: FloatingActive::No,
            original_output,
            scale,
            transform: output.current_transform(),
            view_size,
            working_area,
            shadow: Shadow::new(shadow_config),
            background_buffer: SolidColorBuffer::new(view_size, options.layout.background_color),
            output: Some(output),
            clock,
            base_options,
            options,
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
    pub fn new_scratchpad(clock: Clock, options: Rc<Options>) -> Self {
        let mut workspace = Self::new_with_config_no_outputs(None, clock, options);
        workspace.identity = WorkspaceIdentity::Anonymous;
        workspace
    }

    pub fn new_with_config_no_outputs(
        mut config: Option<WorkspaceConfig>,
        clock: Clock,
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
        let working_area = Rectangle::from_size(Size::from((1280., 720.)));

        let space = TreeSpace::new(
            view_size,
            working_area,
            scale.fractional_scale(),
            clock.clone(),
            options.clone(),
        );

        let floating = FloatingSpace::new();

        let shadow_config =
            compute_workspace_shadow_config(options.overview.workspace_shadow, view_size);

        Self {
            space,
            floating,
            floating_is_active: FloatingActive::No,
            output: None,
            scale,
            transform: Transform::Normal,
            original_output,
            view_size,
            working_area,
            shadow: Shadow::new(shadow_config),
            background_buffer: SolidColorBuffer::new(view_size, options.layout.background_color),
            clock,
            base_options,
            options,
            identity,
            lifetime: WorkspaceLifetime::Persistent,
            layout_config,
            id: WorkspaceId::next(),
        }
    }

    pub fn new_no_outputs(clock: Clock, options: Rc<Options>) -> Self {
        Self::new_with_config_no_outputs(None, clock, options)
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
    fn scratchpad_default_size(&self, tile: &Tile<W>) -> Size<i32, Logical> {
        let workspace_box = self.space.working_area().size;
        let mut size = Size::from((
            (workspace_box.w * 0.5).floor() as i32,
            (workspace_box.h * 0.75).floor() as i32,
        ));

        // `floating_calculate_constraints` on its automatic settings: a floor of 75 by 50, and
        // as the ceiling the box of the whole output layout. The floor is applied outside the
        // ceiling, as sway applies it, so it wins on an output too small to hold it.
        let view_size = self.space.view_size();
        size.w = size.w.min(view_size.w.floor() as i32).max(75);
        size.h = size.h.min(view_size.h.floor() as i32).max(50);

        let min_size = tile.window().min_size();
        let max_size = tile.window().max_size();
        size.w = ensure_min_max_size(size.w, min_size.w, max_size.w);
        size.h = ensure_min_max_size(size.h, min_size.h, max_size.h);
        size
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

    pub(super) fn is_empty_transient_numeric(&self, number: u32) -> bool {
        self.numeric_number() == Some(number)
            && self.lifetime == WorkspaceLifetime::Transient
            && !self.has_windows()
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

    pub fn has_windows_or_persistent_identity(&self) -> bool {
        self.has_windows() || self.has_persistent_identity()
    }

    pub(super) fn should_remove_when_empty(
        &self,
        is_active: bool,
        is_internal_placeholder: bool,
    ) -> bool {
        if is_active || is_internal_placeholder || self.has_windows() {
            return false;
        }

        self.name().is_none() || self.lifetime == WorkspaceLifetime::Transient
    }

    pub fn scale(&self) -> smithay::output::Scale {
        self.scale
    }

    pub fn advance_animations(&mut self) {
        self.space.advance_animations();
        self.floating.advance_animations(&mut self.space);
    }

    pub fn are_animations_ongoing(&self) -> bool {
        self.space.are_animations_ongoing() || self.floating.are_animations_ongoing(&self.space)
    }

    pub fn are_transitions_ongoing(&self) -> bool {
        self.space.are_transitions_ongoing() || self.floating.are_transitions_ongoing(&self.space)
    }

    pub fn update_render_elements(&mut self, is_active: bool) {
        self.space
            .set_active(is_active, self.floating_is_active.get());
        self.space.update_render_elements();

        let view_rect = Rectangle::from_size(self.view_size);
        self.floating
            .update_render_elements(&mut self.space, is_active, view_rect);

        self.shadow.update_render_elements(
            self.view_size,
            true,
            CornerRadius::default(),
            self.scale.fractional_scale(),
            1.,
        );
    }

    pub fn update_config(&mut self, base_options: Rc<Options>) {
        let scale = self.scale.fractional_scale();
        let options = Rc::new(
            Options::clone(&base_options)
                .with_merged_layout(self.layout_config.as_ref())
                .adjusted_for_scale(scale),
        );

        self.space.update_config(
            self.view_size,
            self.working_area,
            self.scale.fractional_scale(),
            options.clone(),
        );

        self.floating.update_config(
            &mut self.space,
            self.view_size,
            self.working_area,
            self.scale.fractional_scale(),
            options.clone(),
        );

        let shadow_config =
            compute_workspace_shadow_config(options.overview.workspace_shadow, self.view_size);
        self.shadow.update_config(shadow_config);

        self.background_buffer
            .set_color(options.layout.background_color);

        self.base_options = base_options;
        self.options = options;
    }

    pub fn update_layout_config(&mut self, layout_config: Option<tiri_config::LayoutPart>) {
        if self.layout_config == layout_config {
            return;
        }

        self.layout_config = layout_config;
        self.update_config(self.base_options.clone());
    }

    pub fn update_shaders(&mut self) {
        self.space.update_shaders();
        self.floating.update_shaders(&mut self.space);
        self.shadow.update_shaders();
    }

    pub fn windows(&self) -> impl Iterator<Item = &W> + '_ {
        self.tiles().map(Tile::window)
    }

    pub fn windows_mut(&mut self) -> impl Iterator<Item = &mut W> + '_ {
        self.tiles_mut().map(Tile::window_mut)
    }

    pub fn tiles(&self) -> impl Iterator<Item = &Tile<W>> + '_ {
        let space = self.space.tiles();
        let floating = self.floating.tiles(&self.space);
        space.chain(floating)
    }

    pub fn tiles_mut(&mut self) -> impl Iterator<Item = &mut Tile<W>> + '_ {
        let tree = self.space.tree_mut();
        let keys = tree.dfs_leaf_keys();
        tree.tiles_mut_for_keys(&keys)
            .into_iter()
            .map(|(_, tile)| tile)
    }

    pub fn is_floating(&self, id: &W::Id) -> bool {
        self.floating.has_window(&self.space, id)
    }

    fn is_floating_target(&self, window: Option<&W::Id>) -> bool {
        window.map_or(self.floating_is_active.get(), |id| {
            self.floating.has_window(&self.space, id)
        })
    }

    /// The one node commands address. Branch and node kind are properties of this key, never
    /// parallel routing state.
    fn command_target_key(&self) -> NodeKey {
        self.space
            .tree()
            .selected_node_key()
            .unwrap_or_else(|| self.space.tree().workspace_root())
    }

    /// Keep the render/input layer aligned after a tree mutation changes selection. Selecting
    /// the workspace itself deliberately preserves the last active side; every other node names
    /// its side through ancestry.
    fn sync_active_layer_to_command_target(&mut self) {
        let target = self.command_target_key();
        if target == self.space.tree().workspace_root() {
            return;
        }
        self.floating_is_active = if self.space.tree().is_floating(target) {
            FloatingActive::Yes
        } else {
            FloatingActive::No
        };
    }

    /// Route a focus command to the active layer.
    ///
    /// Centralizes the tiling-side `activate_tiling_content()` follow-up so it
    /// can never be forgotten by an individual focus method — the historical source of stale
    /// workspace-context bugs.
    fn dispatch_focus(
        &mut self,
        floating: impl FnOnce(&mut FloatingSpace<W>, &mut TreeSpace<W>) -> bool,
        tiling: impl FnOnce(&mut TreeSpace<W>) -> bool,
    ) -> bool {
        let target = self.command_target_key();
        if target == self.space.tree().workspace_root() {
            false
        } else if self.space.tree().is_floating(target) {
            floating(&mut self.floating, &mut self.space)
        } else {
            let moved = tiling(&mut self.space);
            self.activate_tiling_content();
            moved
        }
    }

    /// Route a directional move to the active layer. The floating layer always reports the move
    /// as handled; the tiling layer reports whether it actually moved.
    fn dispatch_move_directional(
        &mut self,
        floating: impl FnOnce(&mut FloatingSpace<W>, &mut TreeSpace<W>),
        tiling: impl FnOnce(&mut TreeSpace<W>) -> bool,
    ) -> bool {
        let target = self.command_target_key();
        if target == self.space.tree().workspace_root() {
            false
        } else if self.space.tree().is_floating(target) {
            floating(&mut self.floating, &mut self.space);
            true
        } else {
            tiling(&mut self.space)
        }
    }

    /// Route by the active layer only, ignoring workspace focus elevation: these commands
    /// resolve their own target inside the layer (i3 falls back to the focused leaf).
    fn dispatch_active_layer<R>(
        &mut self,
        floating: impl FnOnce(&mut FloatingSpace<W>, &mut TreeSpace<W>) -> R,
        tiling: impl FnOnce(&mut TreeSpace<W>) -> R,
    ) -> R {
        if self.floating_is_active.get() {
            floating(&mut self.floating, &mut self.space)
        } else {
            tiling(&mut self.space)
        }
    }

    /// Route by the layer `window` lives in, defaulting to the active layer for `None`.
    fn dispatch_for_window<R>(
        &mut self,
        window: Option<&W::Id>,
        floating: impl FnOnce(&mut FloatingSpace<W>, &mut TreeSpace<W>) -> R,
        tiling: impl FnOnce(&mut TreeSpace<W>) -> R,
    ) -> R {
        if self.is_floating_target(window) {
            floating(&mut self.floating, &mut self.space)
        } else {
            tiling(&mut self.space)
        }
    }

    /// Route a container move. Only the tiling layer reorders containers; the floating layer and
    /// the workspace itself ignore it.
    fn dispatch_move_container<R: Default>(
        &mut self,
        tiling: impl FnOnce(&mut TreeSpace<W>) -> R,
    ) -> R {
        let target = self.command_target_key();
        if target != self.space.tree().workspace_root() && !self.space.tree().is_floating(target) {
            tiling(&mut self.space)
        } else {
            R::default()
        }
    }

    pub fn focus_mode_toggle_targets_floating(&self) -> bool {
        !self.space.tree().is_floating(self.command_target_key())
    }

    pub fn current_output(&self) -> Option<&Output> {
        self.output.as_ref()
    }

    pub fn active_window(&self) -> Option<&W> {
        if self.floating_is_active.get() {
            self.floating.active_window(&self.space)
        } else {
            self.space.active_window()
        }
    }

    pub fn active_window_mut(&mut self) -> Option<&mut W> {
        if self.floating_is_active.get() {
            self.floating.active_window_mut(&mut self.space)
        } else {
            self.space.active_window_mut()
        }
    }

    pub fn active_selection_is_container(&self) -> bool {
        let target = self.command_target_key();
        target != self.space.tree().workspace_root() && !self.space.tree().is_leaf(target)
    }

    pub fn active_command_can_fullscreen(&self) -> bool {
        self.command_target_key() != self.space.tree().workspace_root()
    }

    pub fn close_window_ids_for_active_selection(&self) -> Vec<W::Id> {
        let target = self.command_target_key();
        if target == self.space.tree().workspace_root() {
            return self.windows().map(|window| window.id().clone()).collect();
        }

        let ids = if self.space.tree().is_floating(target) {
            self.floating
                .close_window_ids_for_active_selection(&self.space)
        } else {
            self.space.close_window_ids_for_active_selection()
        };
        if !ids.is_empty() {
            return ids;
        }

        self.active_window()
            .map(|window| vec![window.id().clone()])
            .unwrap_or_default()
    }

    pub fn is_active_pending_fullscreen(&self) -> bool {
        self.space.is_active_pending_fullscreen()
    }

    pub fn set_output(&mut self, output: Option<Output>) {
        if self.output == output {
            return;
        }

        if let Some(output) = self.output.take() {
            for win in self.windows() {
                win.output_leave(&output);
            }
        }

        self.output = output;

        if let Some(output) = &self.output {
            // Normalize original output: possibly replace connector with make/model/serial.
            if self.original_output.matches(output) {
                self.original_output = OutputId::new(output);
            }

            self.update_output_size();

            for win in self.windows() {
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

    pub fn update_output_size(&mut self) {
        let output = self.output.as_ref().unwrap();
        let scale = output.current_scale();
        let transform = output.current_transform();
        let view_size = output_size(output);
        let working_area = compute_working_area(output);
        self.set_view_size(scale, transform, view_size, working_area);
    }

    fn set_view_size(
        &mut self,
        scale: smithay::output::Scale,
        transform: Transform,
        size: Size<f64, Logical>,
        working_area: Rectangle<f64, Logical>,
    ) {
        let scale_transform_changed = self.transform != transform
            || self.scale.integer_scale() != scale.integer_scale()
            || self.scale.fractional_scale() != scale.fractional_scale();
        if !scale_transform_changed && self.view_size == size && self.working_area == working_area {
            return;
        }

        let fractional_scale_changed = self.scale.fractional_scale() != scale.fractional_scale();

        self.scale = scale;
        self.transform = transform;
        self.view_size = size;
        self.working_area = working_area;

        if fractional_scale_changed {
            // Options need to be recomputed for the new scale.
            self.update_config(self.base_options.clone());
        } else {
            // Pass our existing options as is.
            self.space.update_config(
                size,
                working_area,
                scale.fractional_scale(),
                self.options.clone(),
            );
            self.floating.update_config(
                &mut self.space,
                size,
                working_area,
                scale.fractional_scale(),
                self.options.clone(),
            );

            let shadow_config =
                compute_workspace_shadow_config(self.options.overview.workspace_shadow, size);
            self.shadow.update_config(shadow_config);
        }

        self.background_buffer.resize(size);

        if scale_transform_changed {
            for window in self.windows() {
                window.set_preferred_scale_transform(self.scale, self.transform);
            }
        }
    }

    pub fn view_size(&self) -> Size<f64, Logical> {
        self.view_size
    }

    pub fn make_tile(&self, window: W) -> Tile<W> {
        Tile::new(
            window,
            self.view_size,
            self.scale.fractional_scale(),
            self.clock.clone(),
            self.options.clone(),
        )
    }

    pub fn add_tile(
        &mut self,
        mut tile: Tile<W>,
        target: WorkspaceAddWindowTarget<W>,
        activate: ActivateWindow,
        width: ColumnWidth,
        is_full_width: bool,
        is_floating: bool,
    ) {
        self.enter_output_for_window(tile.window());
        let floating_active = self.floating_is_active.get();
        let command_target = self.command_target_key();
        let workspace_command_context = command_target == self.space.tree().workspace_root();
        let command_targets_floating = self.space.tree().is_floating(command_target);
        let command_targets_floating_leaf =
            command_targets_floating && self.space.tree().is_leaf(command_target);
        let command_targets_floating_container =
            command_targets_floating && !self.space.tree().is_leaf(command_target);
        // A tile that is pending maximized or fullscreen has to open in the tiling layout,
        // which is the only side that can do that.
        let can_open_floating =
            tile.window().pending_sizing_mode().is_normal() && !tile.pending_maximized;

        match target {
            WorkspaceAddWindowTarget::Auto => {
                let has_floating_reinsert_hint = tile.floating_reinsert_hint.is_some();
                // Model rule: only a focused floating window inside an explicitly
                // split/grouped floating container auto-groups a newly mapped window.
                // A tile returning from an interactive move carries its original floating
                // parent explicitly and must not be absorbed by whichever group is active now.
                let grouped_floating = !has_floating_reinsert_hint
                    && floating_active
                    && !self
                        .floating
                        .active_container_is_workspace_floated(&self.space)
                    && self.floating.active_container_allows_splits(&self.space)
                    && (command_targets_floating_leaf
                        || self.floating.active_wrapper_selected(&self.space));
                let wants_floating = is_floating || grouped_floating;
                mark_restore_to_floating(&mut tile, wants_floating);

                let keep_floating_focus = floating_active
                    && !wants_floating
                    && (workspace_command_context || command_targets_floating_container);
                // Model rule: when a floating container is selected (focus-parent context),
                // opening a new floating window inserts into that container without stealing
                // selection/focus from the container command target.
                let keep_floating_container_selection = floating_active
                    && wants_floating
                    && self.floating.selected_is_container(&self.space, None);
                let activate = if keep_floating_focus || keep_floating_container_selection {
                    false
                } else if !wants_floating && self.space.has_fullscreen_window() {
                    // Model rule: while a tiling window is fullscreen, newly opened tiling windows
                    // should not steal focus.
                    false
                } else {
                    // Don't steal focus from an active fullscreen window.
                    activate.map_smart(|| !self.is_active_pending_fullscreen())
                };

                if wants_floating && can_open_floating {
                    if has_floating_reinsert_hint {
                        self.floating
                            .add_tile_with_restore_hint(&mut self.space, tile, activate);
                    } else if grouped_floating {
                        self.floating
                            .add_tile_to_active_container(&mut self.space, tile, activate);
                    } else {
                        self.floating.add_tile(&mut self.space, tile, activate);
                    }

                    if activate || self.space.is_empty() {
                        self.activate_floating_for_new_content();
                    }
                } else {
                    let tiling_was_empty = self.space.is_empty();
                    self.space
                        .add_tile(None, tile, activate, width, is_full_width, None);

                    if activate
                        || (floating_active
                            && tiling_was_empty
                            && !wants_floating
                            && !workspace_command_context)
                    {
                        self.activate_tiling_for_new_content();
                    }
                }
            }
            WorkspaceAddWindowTarget::NewColumnAt(col_idx) => {
                mark_restore_to_floating(&mut tile, is_floating);
                let activate = activate.map_smart(|| false);
                self.space
                    .add_tile(Some(col_idx), tile, activate, width, is_full_width, None);

                if activate {
                    self.activate_tiling_for_new_content();
                }
            }
            WorkspaceAddWindowTarget::NextTo(next_to) => {
                let floating_has_window = self.floating.has_window(&self.space, next_to);
                let grouped_floating_target = floating_has_window
                    && self.floating.container_allows_splits(&self.space, next_to);
                let wants_floating = is_floating || grouped_floating_target;
                mark_restore_to_floating(&mut tile, wants_floating);

                let activate = activate
                    .map_smart(|| self.active_window().is_some_and(|win| win.id() == next_to));

                if wants_floating && can_open_floating {
                    if grouped_floating_target {
                        self.floating.add_tile_to_container_of(
                            &mut self.space,
                            next_to,
                            tile,
                            activate,
                        );
                    } else if floating_has_window {
                        self.floating
                            .add_tile_above(&mut self.space, next_to, tile, activate);
                    } else {
                        self.center_new_floating_tile_on(&mut tile, next_to);
                        self.floating.add_tile(&mut self.space, tile, activate);
                    }

                    if activate || self.space.is_empty() {
                        self.activate_floating_for_new_content();
                    }
                } else if floating_has_window {
                    self.space
                        .add_tile(None, tile, activate, width, is_full_width, None);

                    if activate {
                        self.activate_tiling_for_new_content();
                    }
                } else {
                    if self.space.tiles().any(|tile| tile.window().id() == next_to) {
                        self.space
                            .add_tile_right_of(next_to, tile, activate, width, is_full_width);
                    } else {
                        error!("next_to target disappeared while placing a new tiled window");
                        self.space
                            .add_tile(None, tile, activate, width, is_full_width, None);
                    }

                    if activate {
                        // The only add path that also records the restore chain. Nothing has
                        // measured whether the other two are the ones that are wrong: sway has
                        // no such stack, so only a recording of what it focuses after an open
                        // can settle it.
                        self.floating_is_active = FloatingActive::No;
                        self.activate_tiling_content();
                    }
                }
            }
        }
    }

    /// Place a new floating tile centred over the tiled window it belongs to.
    ///
    /// Think a dialog opening on top of its parent.
    fn center_new_floating_tile_on(&self, tile: &mut Tile<W>, next_to: &W::Id) {
        let Some((next_to_tile, render_pos, _visible)) = self
            .space
            .tiles_with_render_positions()
            .find(|(tile, _, _)| tile.window().id() == next_to)
        else {
            error!("next_to target disappeared while placing a new floating window");
            return;
        };

        // FIXME: use static pos
        let tile_size = tile.tile_size();
        let pos =
            render_pos + (next_to_tile.tile_size().to_point() - tile_size.to_point()).downscale(2.);
        let pos =
            self.floating
                .clamp_within_working_area(self.space.working_area(), pos, tile_size);
        let pos = self
            .floating
            .logical_to_size_frac(self.space.working_area(), pos);
        tile.floating_pos = Some(pos);
    }

    pub fn add_tile_to_root_container(
        &mut self,
        root_idx: usize,
        tile_idx: Option<usize>,
        mut tile: Tile<W>,
        activate: bool,
    ) {
        tile.set_scratchpad(false);
        self.enter_output_for_window(tile.window());
        self.space
            .add_tile_to_root_container(root_idx, tile_idx, tile, activate);

        if activate {
            self.floating_is_active = FloatingActive::No;
            self.activate_tiling_content();
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

    pub(super) fn tiling_insert_parent_info(&self, window: &W::Id) -> Option<InsertParentInfo> {
        self.space.insert_parent_info_for_window(window)
    }

    fn inactive_tiling_reference(&self) -> Option<InactiveTilingReference> {
        let debug_restore = std::env::var_os("TIRI_PARITY_DEBUG_RESTORE").is_some();
        let reference = self.space.inactive_tiling_reference();
        if debug_restore {
            eprintln!("restore_target: from_seat_order={reference:?}");
        }
        reference
    }

    fn activate_tiling_content(&mut self) {
        self.space.clear_workspace_selection();
    }

    fn focus_tiling_key(&mut self, key: super::container::NodeKey) -> bool {
        let focused = self.space.focus_inactive_tiling_key(key);
        if focused {
            self.floating_is_active = FloatingActive::No;
            self.activate_tiling_content();
        }
        focused
    }

    fn window_has_fullscreen_focus_scope(&self, window: &W) -> bool {
        self.space.is_fullscreen(window)
            || window.pending_sizing_mode().is_fullscreen()
            || window.is_pending_windowed_fullscreen()
    }

    fn tiling_key_focusable(&self, key: super::container::NodeKey) -> bool {
        let any_fullscreen = self
            .windows()
            .any(|window| self.window_has_fullscreen_focus_scope(window));
        if !any_fullscreen {
            return true;
        }

        self.space
            .window_for_inactive_tiling_key(key)
            .is_some_and(|window| self.window_has_fullscreen_focus_scope(window))
    }

    pub(super) fn focus_floating_window(&mut self, id: &W::Id, raise: bool) -> bool {
        let focused = if raise {
            self.floating.activate_window(&mut self.space, id)
        } else {
            self.floating
                .activate_window_without_raising(&mut self.space, id)
        };
        if focused {
            self.floating_is_active = FloatingActive::Yes;
        }
        focused
    }

    pub(super) fn restore_inactive_floating(&mut self) -> bool {
        let Some(id) = self.space.inactive_floating_window_id() else {
            return false;
        };
        self.focus_floating_window(&id, false)
    }

    pub(super) fn restore_inactive_tiling(&mut self) -> Option<bool> {
        let key = self.space.inactive_tiling_key()?;
        if !self.tiling_key_focusable(key) {
            return Some(false);
        }
        Some(self.focus_tiling_key(key))
    }

    pub(super) fn focus_targets_window(&self, id: &W::Id) -> bool {
        if self.focus_is_elevated() {
            return false;
        }
        self.active_window().is_some_and(|window| window.id() == id)
    }

    /// Swap a dragged tile with the leaf at `target`, sending the displaced tile back to
    /// `origin` (where the dragged tile came from).
    ///
    /// Hands the tile back as `Err` when `target` is no longer a tiling leaf, leaving the
    /// tree untouched so the caller can fall back to a plain insert.
    // The Err variant carries the tile back to the caller; boxing it would only add an
    // allocation to the failure path.
    #[allow(clippy::result_large_err)]
    pub(super) fn tiling_swap_tile(
        &mut self,
        target: NodeKey,
        tile: Tile<W>,
        origin: &InsertParentInfo,
    ) -> Result<(), Tile<W>> {
        if !self.space.is_tiling_leaf(target) {
            return Err(tile);
        }
        let Some(displaced) = self.space.replace_tiling_tile(target, tile) else {
            // is_tiling_leaf just said otherwise; the tile is already gone into the tree.
            return Ok(());
        };
        self.space
            .insert_tile_with_parent_info(origin, displaced, false);
        Ok(())
    }

    pub fn add_tile_split(
        &mut self,
        target: NodeKey,
        direction: Direction,
        mut tile: Tile<W>,
        activate: bool,
    ) -> bool {
        tile.set_scratchpad(false);
        self.enter_output_for_window(tile.window());
        tile.restore_to_floating = false;

        let inserted = self
            .space
            .insert_tile_split(target, direction, tile, activate);

        if inserted && activate {
            self.floating_is_active = FloatingActive::No;
            self.activate_tiling_content();
        }

        inserted
    }

    pub fn add_tile_split_root(
        &mut self,
        direction: Direction,
        mut tile: Tile<W>,
        activate: bool,
    ) -> bool {
        tile.set_scratchpad(false);
        self.enter_output_for_window(tile.window());
        tile.restore_to_floating = false;

        let inserted = self.space.insert_tile_split_root(direction, tile, activate);

        if inserted && activate {
            self.floating_is_active = FloatingActive::No;
            self.activate_tiling_content();
        }

        inserted
    }

    pub fn add_root_tiling_subtree(&mut self, subtree: RootTilingSubtree<W>, activate: bool) {
        for tile in subtree.tiles() {
            self.enter_output_for_window(tile.window());
        }

        self.space
            .add_root_tiling_subtree(None, subtree, activate, None);

        if activate {
            self.floating_is_active = FloatingActive::No;
            self.activate_tiling_content();
        }
    }

    pub fn add_column(&mut self, column: Column<W>, activate: bool) {
        self.add_root_tiling_subtree(column.into(), activate);
    }

    fn update_focus_floating_tiling_after_removing(&mut self, removed_from_floating: bool) {
        // An elevation can only belong to the active layer (the inactive layer's elevation is
        // already dropped by construction), so clear it when that active layer empties out.
        if self.space.is_empty() {
            self.space.clear_selection_context();
        }

        if removed_from_floating {
            if self.floating.is_empty() {
                self.floating_is_active = FloatingActive::No;
                self.activate_tiling_content();
            }
        } else {
            // Tiling should remain focused if both are empty.
            if self.space.is_empty() && !self.floating.is_empty() {
                self.floating_is_active = FloatingActive::Yes;
            }
        }
    }

    pub fn remove_tile(&mut self, id: &W::Id, transaction: Transaction) -> RemovedTile<W> {
        let mut from_floating = false;
        let removed = if self.floating.has_window(&self.space, id) {
            from_floating = true;
            self.floating.remove_tile(&mut self.space, id)
        } else {
            self.space.remove_tile(id, transaction)
        };

        if let Some(output) = &self.output {
            removed.tile.window().output_leave(output);
        }

        self.update_focus_floating_tiling_after_removing(from_floating);

        removed
    }

    pub fn remove_active_tile(&mut self, transaction: Transaction) -> Option<RemovedTile<W>> {
        let from_floating = self.floating_is_active.get();
        let removed = if from_floating {
            self.floating.remove_active_tile(&mut self.space)?
        } else {
            self.space.remove_active_tile(transaction)?
        };

        if let Some(output) = &self.output {
            removed.tile.window().output_leave(output);
        }

        self.update_focus_floating_tiling_after_removing(from_floating);

        Some(removed)
    }

    pub fn remove_active_root_tiling_subtree(&mut self) -> Option<RootTilingSubtree<W>> {
        let from_floating = self.floating_is_active.get();
        if from_floating {
            return None;
        }

        let subtree = self.space.remove_active_root_tiling_subtree()?;

        if let Some(output) = &self.output {
            for tile in subtree.tiles() {
                tile.window().output_leave(output);
            }
        }

        self.update_focus_floating_tiling_after_removing(from_floating);

        Some(subtree)
    }

    pub fn remove_active_tiling_subtree(&mut self) -> Option<RootTilingSubtree<W>> {
        let from_floating = self.floating_is_active.get();
        if from_floating {
            return None;
        }

        let subtree = self.space.remove_active_tiling_subtree()?;

        if let Some(output) = &self.output {
            for tile in subtree.tiles() {
                tile.window().output_leave(output);
            }
        }

        self.update_focus_floating_tiling_after_removing(from_floating);

        Some(subtree)
    }

    pub fn remove_active_column(&mut self) -> Option<Column<W>> {
        self.remove_active_root_tiling_subtree().map(Into::into)
    }

    pub fn resolve_default_width(
        &self,
        default_width: Option<Option<PresetSize>>,
        is_floating: bool,
    ) -> Option<PresetSize> {
        match default_width {
            Some(Some(width)) => Some(width),
            Some(None) => None,
            None if is_floating => None,
            None => self.options.layout.default_column_width,
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
        width: Option<PresetSize>,
        height: Option<PresetSize>,
        is_floating: bool,
        rules: &ResolvedWindowRules,
        (min_size, max_size): (Size<i32, Logical>, Size<i32, Logical>),
    ) -> Size<i32, Logical> {
        let mut size = if is_floating {
            self.floating
                .new_window_size(&self.space, width, height, rules)
        } else {
            self.space.new_window_size(width, height, rules)
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
                state.size = Some(self.view_size.to_i32_round());
            } else if state.states.contains(xdg_toplevel::State::Maximized) {
                state.size = Some(self.working_area.size.to_i32_round());
            } else if !is_floating {
                // Like sway, let an ordinary tiled window choose the geometry it maps with.
                // Mapped stores that geometry as the natural floating size before tiling sends
                // its post-map configure. Sending the tiled size here would erase that
                // distinction for clients that obey their initial configure.
                state.size = None;
            } else {
                let size =
                    self.new_window_size(width, height, is_floating, rules, (min_size, max_size));
                state.size = Some(size);
            }

            if is_floating {
                state.bounds = Some(self.floating.new_window_toplevel_bounds(&self.space, rules));
            } else {
                state.bounds = Some(self.space.new_window_toplevel_bounds(rules));
            }
        });
    }

    pub(super) fn resolve_tiling_width(
        &self,
        window: &W,
        width: Option<PresetSize>,
    ) -> ColumnWidth {
        let width = width.unwrap_or_else(|| PresetSize::Fixed(window.size().w));
        match width {
            PresetSize::Fixed(fixed) => {
                let mut fixed = f64::from(fixed);

                // Add border width since ColumnWidth includes borders.
                let rules = window.rules();
                let border = self.options.layout.border.merged_with(&rules.border);
                if !border.off {
                    fixed += border.width * 2.;
                }

                ColumnWidth::Fixed(fixed as i32)
            }
            PresetSize::Proportion(prop) => ColumnWidth::Proportion(prop),
        }
    }

    pub fn focus_left(&mut self) -> bool {
        self.dispatch_focus(|f, tree| f.focus_left(tree), |t| t.focus_left())
    }

    pub fn focus_left_no_wrap(&mut self) -> bool {
        self.dispatch_focus(
            |f, tree| f.focus_left_no_wrap(tree),
            |t| t.focus_left_no_wrap(),
        )
    }

    pub fn focus_right(&mut self) -> bool {
        self.dispatch_focus(|f, tree| f.focus_right(tree), |t| t.focus_right())
    }

    pub fn focus_right_no_wrap(&mut self) -> bool {
        self.dispatch_focus(
            |f, tree| f.focus_right_no_wrap(tree),
            |t| t.focus_right_no_wrap(),
        )
    }

    pub fn focus_root_container_first(&mut self) {
        self.dispatch_focus(
            |f, tree| {
                f.focus_leftmost(tree);
                true
            },
            |t| {
                t.focus_root_container_first();
                true
            },
        );
    }

    pub fn focus_root_container_last(&mut self) {
        self.dispatch_focus(
            |f, tree| {
                f.focus_rightmost(tree);
                true
            },
            |t| {
                t.focus_root_container_last();
                true
            },
        );
    }

    pub fn focus_column_right_or_first(&mut self) {
        if !self.focus_right() {
            self.focus_root_container_first();
        }
    }

    pub fn focus_column_left_or_last(&mut self) {
        if !self.focus_left() {
            self.focus_root_container_last();
        }
    }

    pub fn focus_column_first(&mut self) {
        self.focus_root_container_first();
    }

    pub fn focus_column_last(&mut self) {
        self.focus_root_container_last();
    }

    pub fn focus_root_container(&mut self, index: usize) {
        if self.floating_is_active.get() {
            self.focus_tiling();
        }
        self.space.focus_root_container(index);
        self.activate_tiling_content();
    }

    pub fn focus_leaf_in_root_container(&mut self, index: u8) {
        if self.floating_is_active.get() {
            return;
        }
        self.space.focus_leaf_in_root_container(index);
        self.activate_tiling_content();
    }

    pub fn focus_column(&mut self, index: usize) {
        self.focus_root_container(index);
    }

    pub fn focus_window_in_column(&mut self, index: u8) {
        self.focus_leaf_in_root_container(index);
    }

    pub fn focus_down(&mut self) -> bool {
        self.dispatch_focus(|f, tree| f.focus_down(tree), |t| t.focus_down())
    }

    pub fn focus_up(&mut self) -> bool {
        self.dispatch_focus(|f, tree| f.focus_up(tree), |t| t.focus_up())
    }

    pub fn focus_down_or_left(&mut self) {
        self.dispatch_focus(
            |f, tree| {
                f.focus_down(tree);
                true
            },
            |t| {
                t.focus_down_or_left();
                true
            },
        );
    }

    pub fn focus_down_or_right(&mut self) {
        self.dispatch_focus(
            |f, tree| {
                f.focus_down(tree);
                true
            },
            |t| {
                t.focus_down_or_right();
                true
            },
        );
    }

    pub fn focus_up_or_left(&mut self) {
        self.dispatch_focus(
            |f, tree| {
                f.focus_up(tree);
                true
            },
            |t| {
                t.focus_up_or_left();
                true
            },
        );
    }

    pub fn focus_up_or_right(&mut self) {
        self.dispatch_focus(
            |f, tree| {
                f.focus_up(tree);
                true
            },
            |t| {
                t.focus_up_or_right();
                true
            },
        );
    }

    pub fn focus_window_top(&mut self) {
        self.dispatch_focus(
            |f, tree| {
                f.focus_topmost(tree);
                true
            },
            |t| {
                t.focus_top();
                true
            },
        );
    }

    pub fn focus_window_bottom(&mut self) {
        self.dispatch_focus(
            |f, tree| {
                f.focus_bottommost(tree);
                true
            },
            |t| {
                t.focus_bottom();
                true
            },
        );
    }

    pub fn focus_window_down_or_top(&mut self) {
        if !self.focus_down() {
            self.focus_window_top();
        }
    }

    pub fn focus_window_up_or_bottom(&mut self) {
        if !self.focus_up() {
            self.focus_window_bottom();
        }
    }

    pub fn focus_up_no_wrap(&mut self) -> bool {
        self.dispatch_focus(|f, tree| f.focus_up_no_wrap(tree), |t| t.focus_up_no_wrap())
    }

    pub fn focus_down_no_wrap(&mut self) -> bool {
        self.dispatch_focus(
            |f, tree| f.focus_down_no_wrap(tree),
            |t| t.focus_down_no_wrap(),
        )
    }

    pub(super) fn focus_entry_from_output_direction(&mut self, direction: Direction) -> bool {
        if self.space.has_fullscreen_window() {
            // Fullscreen workspace targets resolve to the inactive focus under
            // the fullscreen subtree. Keep tiling active as-is.
            self.floating_is_active = FloatingActive::No;
            self.activate_tiling_content();
            return true;
        }

        let Some((root_layout, child_count)) = self.space.root_layout_and_child_count() else {
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
            Direction::Left | Direction::Up => self.space.focus_root_container_last(),
            Direction::Right | Direction::Down => self.space.focus_root_container_first(),
        }
        self.floating_is_active = FloatingActive::No;
        self.activate_tiling_content();
        true
    }

    pub(super) fn has_tiling_windows(&self) -> bool {
        !self.space.is_empty()
    }

    pub(super) fn focus_workspace_node(&mut self) {
        if self.floating.is_empty() {
            self.floating_is_active = FloatingActive::No;
        } else {
            self.floating_is_active = FloatingActive::Yes;
        }
        let _ = self.space.select_root_container();
    }

    fn focus_is_elevated(&self) -> bool {
        self.space.workspace_is_selected()
    }

    /// Switch the active layer to tiling, with the new window as what commands are aimed at.
    ///
    /// Measured against sway 1.11: `focus parent` selects the workspace, but opening a
    /// window ends that — the next command goes to the window. The elevation records that
    /// the user asked for the workspace at some point; a window taking focus answers the
    /// question.
    fn activate_tiling_for_new_content(&mut self) {
        self.floating_is_active = FloatingActive::No;
        self.space.clear_workspace_selection();
    }

    fn activate_floating_for_new_content(&mut self) {
        self.floating_is_active = FloatingActive::Yes;
        self.space.clear_workspace_selection();
    }

    #[cfg(test)]
    pub(super) fn is_floating_workspace_context_active(&self) -> bool {
        self.floating_is_active.get() && self.focus_is_elevated()
    }

    /// Whether the workspace's real node is selected.
    #[cfg(test)]
    pub(super) fn tiling_targets_workspace(&self) -> bool {
        self.space.workspace_is_selected()
    }

    #[cfg(test)]
    pub(super) fn is_tiling_workspace_context_active(&self) -> bool {
        !self.floating_is_active.get() && self.tiling_targets_workspace()
    }

    pub fn focus_window_by_id(&mut self, id: &W::Id) -> bool {
        if self.floating.has_window(&self.space, id)
            && self.floating.focus_window_by_id(&mut self.space, id)
        {
            self.floating_is_active = FloatingActive::Yes;
            return true;
        }

        if self.space.activate_window(id) {
            self.floating_is_active = FloatingActive::No;
            self.activate_tiling_content();
            return true;
        }

        false
    }

    pub fn move_left(&mut self) -> bool {
        self.dispatch_move_directional(|f, tree| f.move_left(tree), |t| t.move_left())
    }

    pub fn move_right(&mut self) -> bool {
        self.dispatch_move_directional(|f, tree| f.move_right(tree), |t| t.move_right())
    }

    pub fn move_container_left(&mut self) -> bool {
        self.dispatch_move_container(|t| t.move_left())
    }

    pub fn move_column_left(&mut self) -> bool {
        self.move_container_left()
    }

    pub fn move_container_right(&mut self) -> bool {
        self.dispatch_move_container(|t| t.move_right())
    }

    pub fn move_column_right(&mut self) -> bool {
        self.move_container_right()
    }

    pub fn move_container_to_first(&mut self) {
        self.dispatch_move_container(|t| t.move_root_container_to_first())
    }

    pub fn move_column_to_first(&mut self) {
        self.move_container_to_first();
    }

    pub fn move_container_to_last(&mut self) {
        self.dispatch_move_container(|t| t.move_root_container_to_last())
    }

    pub fn move_column_to_last(&mut self) {
        self.move_container_to_last();
    }

    pub fn move_container_to_index(&mut self, index: usize) {
        self.dispatch_move_container(|t| t.move_root_container_to_index(index))
    }

    pub fn move_column_to_index(&mut self, index: usize) {
        self.move_container_to_index(index);
    }

    pub fn move_down(&mut self) -> bool {
        self.dispatch_move_directional(|f, tree| f.move_down(tree), |t| t.move_down())
    }

    pub fn move_up(&mut self) -> bool {
        self.dispatch_move_directional(|f, tree| f.move_up(tree), |t| t.move_up())
    }

    pub fn consume_or_expel_window_left(&mut self, window: Option<&W::Id>) {
        self.dispatch_for_window(
            window,
            |f, tree| f.consume_or_expel_window_left(tree, window),
            |t| t.consume_or_expel_window_left(window),
        );
    }

    pub fn consume_or_expel_window_right(&mut self, window: Option<&W::Id>) {
        self.dispatch_for_window(
            window,
            |f, tree| f.consume_or_expel_window_right(tree, window),
            |t| t.consume_or_expel_window_right(window),
        );
    }

    pub fn consume_into_container(&mut self) {
        self.dispatch_active_layer(
            |f, tree| f.consume_into_column(tree),
            |t| t.consume_into_column(),
        );
    }

    pub fn consume_into_column(&mut self) {
        self.consume_into_container();
    }

    pub fn expel_from_container(&mut self) {
        self.dispatch_active_layer(
            |f, tree| f.expel_from_column(tree),
            |t| t.expel_from_column(),
        );
    }

    pub fn expel_from_column(&mut self) {
        self.expel_from_container();
    }

    pub fn swap_window_in_direction(&mut self, direction: Direction) {
        self.dispatch_move_directional(
            |f, tree| f.swap_window_in_direction(tree, direction),
            |t| {
                t.swap_window_in_direction(direction);
                true
            },
        );
    }

    pub fn toggle_column_tabbed_display(&mut self) {
        self.dispatch_active_layer(
            |f, tree| f.toggle_column_tabbed_display(tree),
            |t| t.toggle_column_tabbed_display(),
        );
    }

    pub fn set_column_display(&mut self, display: ColumnDisplay) {
        self.dispatch_active_layer(
            |f, tree| f.set_column_display(tree, display),
            |t| t.set_column_display(display),
        );
    }

    pub fn center_column(&mut self) {
        self.dispatch_active_layer(|f, tree| f.center_window(tree, None), |t| t.center_column());
    }

    pub fn center_window(&mut self, id: Option<&W::Id>) {
        self.dispatch_for_window(
            id,
            |f, tree| f.center_window(tree, id),
            |t| t.center_window(id),
        );
    }

    pub fn center_visible_columns(&mut self) {
        self.dispatch_active_layer(|_, _| {}, |t| t.center_visible_columns());
    }

    pub fn toggle_width(&mut self, forwards: bool) {
        self.dispatch_active_layer(
            |f, tree| f.toggle_window_width(tree, None, forwards),
            |t| t.toggle_width(forwards),
        );
    }

    pub fn toggle_full_width(&mut self) {
        // Floating is left unimplemented for now. For good UX, this probably needs moving the
        // tile to be against the left edge of the working area while it is full-width.
        self.dispatch_active_layer(|_, _| {}, |t| t.toggle_full_width());
    }

    pub fn set_column_width(&mut self, change: SizeChange) {
        self.dispatch_active_layer(
            |f, tree| f.set_window_width(tree, None, change, true),
            |t| t.set_column_width(change),
        );
    }

    pub fn set_window_width(&mut self, window: Option<&W::Id>, change: SizeChange) {
        self.resize_window(
            window,
            ResizeRequest::Axis {
                axis: super::ResizeAxis::Horizontal,
                change,
            },
        );
    }

    pub fn set_window_height(&mut self, window: Option<&W::Id>, change: SizeChange) {
        self.resize_window(
            window,
            ResizeRequest::Axis {
                axis: super::ResizeAxis::Vertical,
                change,
            },
        );
    }

    /// Route one semantic resize request to the active layer.
    pub fn resize_window(&mut self, window: Option<&W::Id>, request: ResizeRequest) {
        self.dispatch_for_window(
            window,
            |f, tree| f.resize_window(tree, window, request),
            |t| t.resize_window(window, request),
        );
    }

    pub fn reset_window_height(&mut self, window: Option<&W::Id>) {
        self.dispatch_for_window(window, |_, _| {}, |t| t.reset_window_height(window));
    }

    pub fn toggle_window_width(&mut self, window: Option<&W::Id>, forwards: bool) {
        self.dispatch_for_window(
            window,
            |f, tree| f.toggle_window_width(tree, window, forwards),
            |t| t.toggle_window_width(window, forwards),
        );
    }

    pub fn toggle_window_height(&mut self, window: Option<&W::Id>, forwards: bool) {
        self.dispatch_for_window(
            window,
            |f, tree| f.toggle_window_height(tree, window, forwards),
            |t| t.toggle_window_height(window, forwards),
        );
    }

    pub fn expand_column_to_available_width(&mut self) {
        self.dispatch_active_layer(|_, _| {}, |t| t.expand_column_to_available_width());
    }

    /// sway's `focus next|prev [sibling]`. Tiling only: a floating window has no parent
    /// laying its siblings out in a direction to read one from.
    pub fn focus_along_parent(&mut self, forward: bool, descend: bool) -> bool {
        self.space.focus_along_parent(forward, descend)
    }

    pub fn focus_parent(&mut self) {
        let target = self.command_target_key();
        if target == self.space.tree().workspace_root() {
            return;
        }
        if self.space.tree().is_floating(target) {
            let _ = self.floating.focus_parent(&mut self.space);
        } else {
            let _ = self.space.focus_parent();
        }
    }

    pub fn focus_child(&mut self) {
        let target = self.command_target_key();
        if target == self.space.tree().workspace_root() {
            if self.floating_is_active.get() {
                let _ = self.floating.focus_child_from_workspace(&mut self.space);
                return;
            }
            if !self.space.is_empty() {
                let _ = self.space.focus_child();
                self.activate_tiling_content();
            }
        } else if self.space.tree().is_floating(target) {
            self.floating.focus_child(&mut self.space);
        } else {
            self.space.focus_child();
            self.activate_tiling_content();
        }
    }

    /// Route a split/layout command: workspace-level targets apply to the workspace layout,
    /// floating targets drop focus elevation unless the selected floating workspace container
    /// preserves it. Model rule: split/layout commands work in both floating and space.
    fn dispatch_layout(
        &mut self,
        workspace: impl FnOnce(&mut TreeSpace<W>),
        tiling: impl FnOnce(&mut TreeSpace<W>),
        floating: impl FnOnce(&mut FloatingSpace<W>, &mut TreeSpace<W>),
    ) {
        let target = self.command_target_key();
        if target == self.space.tree().workspace_root() {
            workspace(&mut self.space);
        } else if self.space.tree().is_floating(target) {
            floating(&mut self.floating, &mut self.space);
        } else {
            tiling(&mut self.space);
        }
        self.sync_active_layer_to_command_target();
    }

    pub fn split_horizontal(&mut self) {
        self.dispatch_layout(
            |t| t.split_workspace_horizontal(),
            |t| t.split_horizontal(),
            |f, tree| f.split_horizontal(tree),
        );
    }

    pub fn split_vertical(&mut self) {
        self.dispatch_layout(
            |t| t.split_workspace_vertical(),
            |t| t.split_vertical(),
            |f, tree| f.split_vertical(tree),
        );
    }

    pub fn split_none(&mut self) {
        self.dispatch_layout(|_| {}, |t| t.split_none(), |f, tree| f.split_none(tree));
    }

    /// `split toggle`, which sway does not implement as an operation of its own.
    ///
    /// `cmd_split` reads the layout of the parent of whatever the command is aimed at and
    /// runs `split h` when it is vertical and `split v` otherwise — including when there is
    /// no parent to read, which is the workspace itself. So this chooses, and everything
    /// after it is the ordinary split path, wrapping and all.
    pub fn split_toggle(&mut self) {
        let parent_is_vertical = self.space.command_target_parent_layout() == Some(Layout::SplitV);
        if parent_is_vertical {
            self.split_horizontal();
        } else {
            self.split_vertical();
        }
    }

    pub fn set_layout_mode(&mut self, layout: Layout) {
        self.dispatch_layout(
            |t| {
                t.set_workspace_layout_mode(layout);
            },
            |t| t.set_layout_mode(layout),
            |f, tree| f.set_layout_mode(tree, layout),
        );
    }

    pub fn toggle_split_layout(&mut self) {
        self.dispatch_layout(
            |t| t.toggle_workspace_split_layout(),
            |t| t.toggle_split_layout(),
            |f, tree| f.toggle_split_layout(tree),
        );
    }

    pub fn toggle_layout_all(&mut self) {
        self.dispatch_layout(
            |t| t.toggle_workspace_layout_all(),
            |t| t.toggle_layout_all(),
            |f, tree| f.toggle_layout_all(tree),
        );
    }

    pub fn set_default_layout(&mut self) {
        self.dispatch_layout(
            |t| t.set_workspace_default_layout(),
            |t| t.set_default_layout(),
            |f, tree| f.set_default_layout(tree),
        );
    }

    pub(super) fn toggle_layout_cycle(&mut self, cycle: &[LayoutCycleEntry]) {
        self.dispatch_layout(
            |t| t.toggle_workspace_layout_cycle(cycle),
            |t| t.toggle_layout_cycle(cycle),
            |f, tree| f.toggle_layout_cycle(tree, cycle),
        );
    }

    /// The zero-or-one window named by sway's workspace-wide fullscreen pointer.
    ///
    /// This stays a `Vec` for the existing inspection API; internally there is only one
    /// `Option<NodeKey>` and the id is derived from its live leaf.
    pub fn fullscreen_window_ids(&self) -> Vec<W::Id> {
        self.space
            .tree()
            .fullscreen_representative_window_id()
            .cloned()
            .into_iter()
            .collect()
    }

    pub fn set_fullscreen(&mut self, window: &W::Id, is_fullscreen: bool) {
        if is_fullscreen {
            // A workspace has one fullscreen window. `container_set_fullscreen` disables the
            // one already there before setting the new one, and it looks it up through
            // `ws->fullscreen`, which spans both of the workspace's lists
            // (sway/tree/container.c:1375-1377). Without this, fullscreening across the two
            // sides leaves tiri with two.
            for id in self.fullscreen_window_ids() {
                if &id != window {
                    self.set_fullscreen(&id, false);
                }
            }
        }

        if self.floating.has_window(&self.space, window) {
            self.floating
                .set_fullscreen(&mut self.space, window, is_fullscreen);
            return;
        }

        if !is_fullscreen {
            // The window is in the tiling layout and we're requesting an unfullscreen. If it is
            // indeed fullscreen (i.e. this isn't a duplicate unfullscreen request), then we may
            // need to unfullscreen into floating.
            let tile = self
                .space
                .tiles()
                .find(|tile| tile.window().id() == window)
                .unwrap();

            // When going from fullscreen to maximized, don't consider restore_to_floating yet.
            // pending_sizing_mode() is asynchronous, so also check space.is_fullscreen() to
            // handle requests while the client is catching up.
            let is_fullscreen_now = self.space.is_fullscreen(tile.window())
                || tile.window().pending_sizing_mode().is_fullscreen();
            if is_fullscreen_now && !tile.pending_maximized && tile.restore_to_floating {
                // Unfullscreen and float in one call so it has a chance to notice and request a
                // (0, 0) size, rather than the tiling tile size.
                self.toggle_window_floating(Some(window));
                return;
            }
        }

        self.space.set_fullscreen(window, is_fullscreen);
    }

    pub fn toggle_fullscreen(&mut self, window: &W::Id) {
        if self.floating.has_window(&self.space, window) {
            let current = self.floating.is_fullscreen(&self.space, window);
            self.set_fullscreen(window, !current);
            return;
        }

        let tile = self
            .tiles()
            .find(|tile| tile.window().id() == window)
            .unwrap();
        // Use space.is_fullscreen() as the source of truth instead of pending_sizing_mode(),
        // which updates asynchronously after animations complete.
        let current = self.space.is_fullscreen(tile.window());
        self.set_fullscreen(window, !current);
    }

    pub fn toggle_fullscreen_for_command(&mut self, _window: &W::Id) {
        // Resolve once to the tree object the command addresses. Floating and tiling are two
        // workspace branches, not two fullscreen semantics: a leaf owns client fullscreen,
        // while a container owns compositor-side fullscreen as the same stable NodeKey.
        let target = self.command_target_key();

        if target == self.space.tree().workspace_root() {
            return;
        }
        if self.space.tree().is_leaf(target) {
            let Some(id) = self
                .space
                .tree()
                .get_tile(target)
                .map(|tile| tile.window().id().clone())
            else {
                return;
            };
            self.toggle_fullscreen(&id);
        } else {
            if self.space.tree().fullscreen_key() != Some(target) {
                // A previous leaf has protocol state to revoke. A previous container does
                // not; replacing the workspace pointer below is sufficient for it.
                if let Some(id) = self.space.tree().fullscreen_leaf_window_id().cloned() {
                    self.set_fullscreen(&id, false);
                }
            }
            self.space.toggle_fullscreen_container(target);
        }
    }

    pub fn set_windowed_fullscreen(&mut self, window: &W::Id, is_fullscreen: bool) {
        self.set_fullscreen(window, is_fullscreen);
    }

    pub fn set_maximized(&mut self, window: &W::Id, maximize: bool) {
        let mut restore_to_floating = false;
        if self.floating.has_window(&self.space, window) {
            if maximize {
                restore_to_floating = true;
                self.space
                    .tree_mut()
                    .discard_layout_superseded_by_transfer();
                self.toggle_window_floating(Some(window));
            } else {
                // Floating windows are never maximized, so this is an unmaximize request for an
                // already unmaximized window.
                return;
            }
        } else if !maximize {
            // The window is in the tiling layout and we're requesting to unmaximize. If it is
            // indeed maximized (i.e. this isn't a duplicate unmaximize request), then we may
            // need to unmaximize into floating.
            let tile = self
                .space
                .tiles()
                .find(|tile| tile.window().id() == window)
                .unwrap();
            if tile.window().pending_sizing_mode().is_fullscreen() {
                self.space.set_maximized(window, maximize);
                return;
            }
            if tile.pending_maximized && tile.restore_to_floating {
                // Unmaximize and float in one call so it has a chance to notice and request a
                // (0, 0) size, rather than the tiling tile size.
                self.toggle_window_floating(Some(window));
                return;
            }
        }

        let tile = self
            .space
            .tiles()
            .find(|tile| tile.window().id() == window)
            .unwrap();
        let was_normal = tile.window().pending_sizing_mode().is_normal();

        self.space.set_maximized(window, maximize);

        // When going from normal to maximized, remember if we should unmaximize to floating.
        let tile = self
            .space
            .tiles_mut()
            .find(|tile| tile.window().id() == window)
            .unwrap();
        if was_normal && tile.pending_maximized {
            tile.restore_to_floating = restore_to_floating;
        }
    }

    pub fn toggle_maximized(&mut self, window: &W::Id) {
        let current = self
            .space
            .tiles()
            .find(|tile| tile.window().id() == window)
            .is_some_and(|tile| tile.pending_maximized);

        self.set_maximized(window, !current);
    }

    pub fn toggle_window_floating(&mut self, id: Option<&W::Id>) {
        let Some(transfer) = self.resolve_floating_transfer(id) else {
            return;
        };
        let id = transfer.window_id().clone();

        // sway disables floating fullscreen before moving the node back to tiling.
        if self.floating.is_fullscreen(&self.space, &id) {
            self.floating.set_fullscreen(&mut self.space, &id, false);
        }

        let render_pos = transfer
            .may_animate_window_position()
            .then(|| {
                self.tiles_with_render_positions()
                    .find(|(tile, _, _)| *tile.window().id() == id)
                    .map(|(_, pos, _)| pos)
            })
            .flatten();
        let animate = match transfer {
            FloatingTransfer::Float(transfer) => self.execute_float_transfer(transfer),
            FloatingTransfer::Unfloat(transfer) => self.execute_unfloat_transfer(transfer),
        };

        if animate {
            if let (Some(render_pos), Some((tile, new_render_pos))) = (
                render_pos,
                self.tiles_with_render_positions_mut(false)
                    .find(|(tile, _)| *tile.window().id() == id),
            ) {
                tile.animate_move_from(render_pos - new_render_pos);
            }
        }
    }

    fn resolve_floating_transfer(
        &mut self,
        requested_id: Option<&W::Id>,
    ) -> Option<FloatingTransfer<W::Id>> {
        let command_target = self.command_target_key();
        let command_targets_workspace = command_target == self.space.tree().workspace_root();
        let command_targets_tiling =
            !command_targets_workspace && !self.space.tree().is_floating(command_target);

        if requested_id.is_none() && command_targets_workspace {
            // A selected workspace with no tiled children has nothing that can become floating.
            if self.space.is_empty() {
                return None;
            }
        }

        let explicit_window = requested_id.is_some();
        let active_id = self.active_window().map(|win| win.id().clone());
        let target_is_active = requested_id.is_none_or(|id| Some(id) == active_id.as_ref());
        let id = requested_id.cloned().or(active_id)?;
        let is_floating = self.floating.has_window(&self.space, &id);
        let inactive_tiling_reference = if is_floating {
            self.inactive_tiling_reference()
        } else {
            None
        };

        if !explicit_window
            && target_is_active
            && command_targets_workspace
            && !self.space.is_empty()
        {
            return Some(FloatingTransfer::Float(FloatTransfer::Workspace {
                focus_id: id,
            }));
        }

        // Model rule: if a tiling container is selected (focus-parent semantics),
        // floating toggle targets that selected container even if floating focus mode
        // is currently active.
        if !explicit_window
            && target_is_active
            && command_targets_tiling
            && self.space.selected_is_container()
        {
            return Some(FloatingTransfer::Float(FloatTransfer::SelectedContainer {
                focus_id: id,
            }));
        }

        if is_floating {
            // `container_set_floating` asks `seat_get_focus_inactive_tiling` where the same
            // node lands (`sway/tree/container.c:1039-1057`). Its old parent is deliberately
            // not a restore target; sway detached it when the node became floating.
            if !explicit_window {
                let was_workspace = self
                    .floating
                    .active_container_is_workspace_floated(&self.space);
                let tiling_was_empty = self.space.is_empty();
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
    fn execute_float_transfer(&mut self, transfer: FloatTransfer<W::Id>) -> bool {
        match transfer {
            FloatTransfer::Workspace { focus_id } => {
                let Some((subtree, rect)) = self.space.take_workspace_subtree_for_floating() else {
                    return false;
                };
                let focus_id = self.focus_id_inside_subtree(&focus_id, subtree);
                if !self.floating.add_subtree(
                    &mut self.space,
                    subtree,
                    rect,
                    true,
                    focus_id.as_ref(),
                    true,
                ) {
                    return false;
                }
                if let Some(focus_id) = focus_id.as_ref() {
                    self.floating
                        .select_wrapper_for_window(&mut self.space, focus_id);
                }
                self.floating_is_active = FloatingActive::Yes;
                false
            }
            FloatTransfer::SelectedContainer { focus_id } => {
                let Some((subtree, rect)) = self.space.take_selected_subtree() else {
                    return false;
                };
                let focus_id = self.focus_id_inside_subtree(&focus_id, subtree);
                if !self.floating.add_subtree(
                    &mut self.space,
                    subtree,
                    rect,
                    true,
                    focus_id.as_ref(),
                    false,
                ) {
                    return false;
                }
                if let Some(focus_id) = focus_id.as_ref() {
                    self.floating
                        .select_wrapper_for_window(&mut self.space, focus_id);
                }
                self.floating_is_active = FloatingActive::Yes;
                false
            }
            FloatTransfer::Window {
                id,
                target_is_active,
            } => {
                let Some((subtree, rect)) = self.space.subtree_for_window_floating(&id) else {
                    return false;
                };
                if let Some(tile) = self.space.tree_mut().get_tile_mut(subtree) {
                    tile.stop_move_animations();
                    tile.pending_maximized = false;
                }

                if !self.floating.add_subtree(
                    &mut self.space,
                    subtree,
                    rect,
                    target_is_active,
                    Some(&id),
                    false,
                ) {
                    return false;
                }
                // The floating side takes over either because the window that moved was the
                // active one, or because floating it emptied the tiled side and left nothing
                // for the active layer to point at.
                if target_is_active || self.space.is_empty() {
                    self.floating_is_active = FloatingActive::Yes;
                }
                true
            }
        }
    }

    /// Execute a resolved floating-to-tiling transfer.
    ///
    /// An implicit command first targets the floating group. The window fallback preserves
    /// the old behaviour if that group vanished between resolution and execution.
    fn execute_unfloat_transfer(&mut self, transfer: UnfloatTransfer<W::Id>) -> bool {
        match transfer {
            UnfloatTransfer::Container {
                id,
                target_is_active,
                tiling_reference,
                was_workspace,
                tiling_was_empty,
            } => {
                if self.floating.unfloat_container(
                    &mut self.space,
                    &id,
                    tiling_reference.as_ref(),
                    was_workspace && tiling_was_empty,
                    target_is_active,
                ) {
                    if target_is_active {
                        self.finish_active_unfloat();
                    }
                    return false;
                }

                let unfloated = self.floating.unfloat_window(
                    &mut self.space,
                    &id,
                    tiling_reference.as_ref(),
                    target_is_active,
                );
                if unfloated && target_is_active {
                    self.finish_active_unfloat();
                }
                unfloated
            }
            UnfloatTransfer::Window {
                id,
                target_is_active,
                tiling_reference,
            } => {
                let unfloated = self.floating.unfloat_window(
                    &mut self.space,
                    &id,
                    tiling_reference.as_ref(),
                    target_is_active,
                );
                if unfloated && target_is_active {
                    self.finish_active_unfloat();
                }
                unfloated
            }
        }
    }

    fn focus_id_inside_subtree(
        &self,
        id: &W::Id,
        subtree: super::container::NodeKey,
    ) -> Option<W::Id> {
        self.space
            .tree()
            .window_key(id)
            .filter(|key| self.space.tree().is_descendant(*key, subtree))
            .map(|_| id.clone())
    }

    fn finish_active_unfloat(&mut self) {
        self.floating_is_active = FloatingActive::No;
        self.activate_tiling_content();
    }

    pub fn scratchpad_window_id(&self) -> Option<W::Id> {
        self.floating
            .tiles(&self.space)
            .find(|tile| tile.is_scratchpad())
            .map(|tile| tile.window().id().clone())
    }

    pub fn take_tile_for_scratchpad(&mut self, id: &W::Id) -> Option<Tile<W>> {
        let removed = self.remove_tile(id, Transaction::new());
        let mut tile = removed.tile;
        tile.set_scratchpad(true);
        tile.window_mut().set_floating(true);

        // `root_scratchpad_add_container`: a window that was already floating is put away
        // exactly as it is, size and position untouched. A tiled one is floated first, and
        // floating it is what decides its size — sway does that with
        // `container_floating_set_default_size` and `container_floating_move_to_center`, not
        // with the natural size a `floating enable` would have given it.
        //
        // sway/tree/root.c:114-119
        if !removed.is_floating {
            tile.stop_move_animations();
            tile.clear_resize_animation();
            tile.pending_maximized = false;
            tile.floating_pos = None;

            let size = self.scratchpad_default_size(&tile);
            tile.floating_window_size = Some(size);
            tile.window_mut().request_size_once(size, false);

            let working_area = self.space.working_area();
            let size_f = Size::from((size.w as f64, size.h as f64));
            let pos = center_preferring_top_left_in_area(working_area, size_f);
            tile.floating_pos = Some(self.floating.logical_to_size_frac(working_area, pos));

            let border_config = self
                .options
                .layout
                .border
                .merged_with(&tile.window().rules().border);
            let bounds = compute_toplevel_bounds(border_config, working_area.size);
            let win = tile.window_mut();
            win.set_bounds(bounds);
            win.send_pending_configure();
            win.refresh();
        }

        Some(tile)
    }

    /// Put a window away, at the back of the round-robin order.
    ///
    /// The order is the floating stack's: a hidden window goes on top, so the one hidden
    /// longest ago is at the bottom, and that is the one [`Self::next_scratchpad_window`]
    /// brings out. A queue, said with the ordering the workspace already has rather than with
    /// one beside it.
    pub fn hide_in_scratchpad(&mut self, tile: Tile<W>) {
        self.add_scratchpad_tile(tile, false);
    }

    /// The window `scratchpad show` would bring out next: the one hidden longest ago.
    pub fn next_scratchpad_window(&self) -> Option<W::Id> {
        self.windows().last().map(|window| window.id().clone())
    }

    pub fn add_scratchpad_tile(&mut self, mut tile: Tile<W>, activate: bool) {
        tile.set_scratchpad(true);
        tile.window_mut().set_floating(true);
        self.enter_output_for_window(tile.window());
        self.floating.add_tile(&mut self.space, tile, activate);

        if activate || self.space.is_empty() {
            self.floating_is_active = FloatingActive::Yes;
        }
    }

    pub fn set_window_floating(&mut self, id: Option<&W::Id>, floating: bool) {
        if self.is_floating_target(id) == floating {
            return;
        }

        self.toggle_window_floating(id);
    }

    pub fn focus_floating(&mut self) {
        if !self.floating_is_active.get() {
            self.switch_focus_floating_tiling();
        }
    }

    pub fn focus_tiling(&mut self) {
        if self.floating_is_active.get() {
            self.switch_focus_floating_tiling();
        }
    }

    pub fn switch_focus_floating_tiling(&mut self) {
        if self.floating.is_empty() || self.space.is_empty() {
            return;
        }

        self.space.clear_selection_context();
        let was_floating_active = self.floating_is_active.get();
        self.floating_is_active = if was_floating_active {
            FloatingActive::No
        } else {
            FloatingActive::Yes
        };
        if !self.floating_is_active.get() {
            self.activate_tiling_content();
        }
    }

    pub fn clear_selection_context(&mut self) {
        self.space.clear_selection_context();
    }

    pub fn move_floating_window(
        &mut self,
        id: Option<&W::Id>,
        x: PositionChange,
        y: PositionChange,
        animate: bool,
    ) {
        if self.is_floating_target(id) {
            self.floating
                .move_window(&mut self.space, id, x, y, animate);
        } else {
            // If the target tile isn't floating, set its stored floating position.
            let working_area = self.space.working_area();
            let tile = if let Some(id) = id {
                self.space
                    .tiles_mut()
                    .find(|tile| tile.window().id() == id)
                    .unwrap()
            } else if let Some(tile) = self.space.active_tile_mut() {
                tile
            } else {
                return;
            };

            let pos = self.floating.stored_or_default_tile_pos(working_area, tile);

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

            let pos = self.floating.logical_to_size_frac(working_area, pos);
            tile.floating_pos = Some(pos);
        }
    }

    pub fn has_windows(&self) -> bool {
        self.windows().next().is_some()
    }

    pub fn has_window(&self, window: &W::Id) -> bool {
        self.windows().any(|win| win.id() == window)
    }

    pub fn find_wl_surface(&self, wl_surface: &WlSurface) -> Option<&W> {
        self.windows().find(|win| win.is_wl_surface(wl_surface))
    }

    pub fn find_wl_surface_mut(&mut self, wl_surface: &WlSurface) -> Option<&mut W> {
        self.windows_mut().find(|win| win.is_wl_surface(wl_surface))
    }

    pub fn tiles_with_render_positions(
        &self,
    ) -> impl Iterator<Item = (&Tile<W>, Point<f64, Logical>, bool)> {
        let fullscreen_scope = self.floating.fullscreen_key(&self.space);
        let space = self
            .space
            .tiles_with_render_positions()
            .map(move |(tile, pos, visible)| (tile, pos, visible && fullscreen_scope.is_none()));

        let floating = self.floating.tiles_with_render_positions(&self.space);
        let visible = self.is_floating_visible();
        let tree = self.space.tree();
        let floating = floating.map(move |(tile, pos)| {
            let in_scope =
                fullscreen_scope.is_none_or(|scope| tree.is_descendant(tile.node_key(), scope));
            (tile, pos, visible && in_scope)
        });

        floating.chain(space)
    }

    pub fn tiles_with_render_positions_mut(
        &mut self,
        round: bool,
    ) -> impl Iterator<Item = (&mut Tile<W>, Point<f64, Logical>)> {
        let scale = self.scale.fractional_scale();
        let layouts: Vec<_> = self
            .space
            .tree()
            .leaf_layouts()
            .iter()
            .map(|info| (info.key, info.rect.loc))
            .collect();
        let keys: Vec<_> = layouts.iter().map(|(key, _)| *key).collect();
        let locs: Vec<_> = layouts.iter().map(|(_, loc)| *loc).collect();
        self.space
            .tree_mut()
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

    pub fn tiles_with_ipc_layouts(&self) -> impl Iterator<Item = (&Tile<W>, WindowLayout)> {
        let space = self.space.tiles_with_ipc_layouts();
        let floating = self.floating.tiles_with_ipc_layouts(&self.space);
        floating.chain(space)
    }

    pub fn active_window_visual_rectangle(&self) -> Option<Rectangle<f64, Logical>> {
        if self.floating_is_active.get() {
            self.floating.active_window_visual_rectangle(&self.space)
        } else {
            self.space.active_tile_visual_rectangle()
        }
    }

    pub fn popup_target_rect(&self, window: &W::Id) -> Option<Rectangle<f64, Logical>> {
        if self.floating.has_window(&self.space, window) {
            self.floating.popup_target_rect(&self.space, window)
        } else {
            self.space.popup_target_rect(window)
        }
    }

    pub fn render_tiling<R: NiriRenderer>(
        &self,
        ctx: RenderCtx<R>,
        xray_pos: XrayPos,
        focus_ring: bool,
        push: &mut dyn FnMut(WorkspaceRenderElement<R>),
    ) {
        if self.floating.fullscreen_key(&self.space).is_some() {
            return;
        }
        let tiling_focus_ring = focus_ring && !self.floating_is_active();
        self.space
            .render(ctx, xray_pos, tiling_focus_ring, &mut |elem| {
                push(elem.into())
            });
    }

    pub fn render_tiling_as_offscreen<R: NiriRenderer>(
        &self,
        renderer: &mut GlesRenderer,
        target: RenderTarget,
        focus_ring: bool,
        push: &mut dyn FnMut(WorkspaceRenderElement<R>),
    ) {
        if self.floating.fullscreen_key(&self.space).is_some() {
            return;
        }
        let tiling_focus_ring = focus_ring && !self.floating_is_active();
        if let Some(elem) = self
            .space
            .render_as_offscreen(renderer, target, tiling_focus_ring)
        {
            push(elem.into());
        }
    }

    pub fn render_floating<R: NiriRenderer>(
        &self,
        ctx: RenderCtx<R>,
        xray_pos: XrayPos,
        focus_ring: bool,
        push: &mut dyn FnMut(WorkspaceRenderElement<R>),
    ) {
        if !self.is_floating_visible() {
            return;
        }

        let view_rect = Rectangle::from_size(self.view_size);
        let floating_focus_ring = focus_ring && self.floating_is_active();
        self.floating.render(
            &self.space,
            ctx,
            xray_pos,
            view_rect,
            floating_focus_ring,
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

    pub fn render_above_top_layer(&self) -> bool {
        self.space.render_above_top_layer()
            || self
                .floating
                .fullscreen_key(&self.space)
                .is_some_and(|key| {
                    self.space.tree().container_info(key).is_some()
                        || self
                            .space
                            .tree()
                            .get_tile(key)
                            .is_some_and(|tile| tile.window().sizing_mode().is_fullscreen())
                })
    }

    pub fn is_floating_visible(&self) -> bool {
        // If the focus is on a fullscreen tiling window, hide the floating windows.
        matches!(
            self.floating_is_active,
            FloatingActive::Yes | FloatingActive::NoButRaised
        ) || self.floating.fullscreen_key(&self.space).is_some()
            || !self.render_above_top_layer()
    }

    pub fn store_unmap_snapshot_if_empty(
        &mut self,
        renderer: &mut GlesRenderer,
        xray: Option<&mut Xray>,
        xray_has_blocked_out_layers: bool,
        xray_pos: XrayPos,
        window: &W::Id,
    ) {
        let view_size = self.view_size();
        for (tile, tile_pos) in self.tiles_with_render_positions_mut(false) {
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

    pub fn clear_unmap_snapshot(&mut self, window: &W::Id) {
        for tile in self.tiles_mut() {
            if tile.window().id() == window {
                let _ = tile.take_unmap_snapshot();
                return;
            }
        }
    }

    pub fn start_close_animation_for_window(
        &mut self,
        renderer: &mut GlesRenderer,
        window: &W::Id,
        blocker: TransactionBlocker,
    ) {
        if self.floating.has_window(&self.space, window) {
            self.floating.start_close_animation_for_window(
                &mut self.space,
                renderer,
                window,
                blocker,
            );
        } else {
            self.space
                .start_close_animation_for_window(renderer, window, blocker);
        }
    }

    pub fn start_close_animation_for_tile(
        &mut self,
        renderer: &mut GlesRenderer,
        snapshot: TileRenderSnapshot,
        tile_size: Size<f64, Logical>,
        tile_pos: Point<f64, Logical>,
        blocker: TransactionBlocker,
    ) {
        self.floating.start_close_animation_for_tile(
            &self.space,
            renderer,
            snapshot,
            tile_size,
            tile_pos,
            blocker,
        );
    }

    pub fn start_open_animation(&mut self, id: &W::Id) -> bool {
        self.space.start_open_animation(id)
            || self.floating.start_open_animation(&mut self.space, id)
    }

    pub fn window_under(&self, pos: Point<f64, Logical>) -> Option<(&W, HitType)> {
        if self.is_floating_visible() {
            if let Some(rv) = self.floating.window_under(&self.space, pos) {
                return Some(rv);
            }
        }

        self.space.window_under(pos)
    }

    pub fn resize_edges_under(&mut self, pos: Point<f64, Logical>) -> Option<ResizeEdge> {
        self.resize_hit_under(pos).map(|hit| hit.edges)
    }

    pub fn resize_hit_under(&mut self, pos: Point<f64, Logical>) -> Option<ResizeHit<W::Id>> {
        if self.is_active_pending_fullscreen() {
            return None;
        }

        if self.is_floating_visible() {
            match self.floating.resize_hit_under(&self.space, pos) {
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
            if self.floating_is_active() {
                return None;
            }
        }

        self.space.resize_hit_under(pos)
    }

    pub fn descendants_added(&mut self, id: &W::Id) -> bool {
        self.floating.descendants_added(&self.space, id)
    }

    pub fn update_window(&mut self, window: &W::Id, serial: Option<Serial>) {
        if !self.floating.update_window(&mut self.space, window, serial) {
            self.space.update_window(window, serial);
        }
    }

    pub fn refresh(&mut self, is_active: bool, is_focused: bool) {
        self.space
            .refresh(is_active && !self.floating_is_active.get(), is_focused);
        self.floating.refresh(
            &mut self.space,
            is_active && self.floating_is_active.get(),
            is_focused,
        );
    }

    pub fn activation_view_distance(&self, window: &W::Id) -> f64 {
        if self.floating.has_window(&self.space, window) {
            return 0.;
        }

        self.space.activation_view_distance(window)
    }

    pub fn is_urgent(&self) -> bool {
        self.windows().any(|win| win.is_urgent())
    }

    pub fn activate_window(&mut self, window: &W::Id) -> bool {
        if self.floating.activate_window(&mut self.space, window) {
            self.floating_is_active = FloatingActive::Yes;
            true
        } else if self.space.activate_window(window) {
            self.floating_is_active = FloatingActive::No;
            self.activate_tiling_content();
            true
        } else {
            false
        }
    }

    pub fn activate_window_without_raising(&mut self, window: &W::Id) -> bool {
        if self
            .floating
            .activate_window_without_raising(&mut self.space, window)
        {
            self.floating_is_active = FloatingActive::Yes;
            true
        } else if self.space.activate_window(window) {
            self.floating_is_active = match self.floating_is_active {
                FloatingActive::No => FloatingActive::No,
                FloatingActive::NoButRaised => FloatingActive::NoButRaised,
                FloatingActive::Yes => FloatingActive::NoButRaised,
            };
            self.activate_tiling_content();
            true
        } else {
            false
        }
    }

    pub(super) fn tiling_insert_position(&self, pos: Point<f64, Logical>) -> InsertPosition {
        self.space.insert_position(pos)
    }

    pub(super) fn insert_hint_area(
        &self,
        position: &InsertPosition,
    ) -> Option<Rectangle<f64, Logical>> {
        self.space.insert_hint_area(position)
    }

    pub fn horizontal_view_gesture_begin(&mut self, is_touchpad: bool) {
        self.space.horizontal_view_gesture_begin(is_touchpad);
    }

    pub fn horizontal_view_gesture_update(
        &mut self,
        delta_x: f64,
        timestamp: Duration,
        is_touchpad: bool,
    ) -> Option<bool> {
        self.space
            .horizontal_view_gesture_update(delta_x, timestamp, is_touchpad)
    }

    pub fn horizontal_view_gesture_end(&mut self, is_touchpad: Option<bool>) -> bool {
        self.space.horizontal_view_gesture_end(is_touchpad)
    }

    pub fn interactive_resize_begin(&mut self, window: W::Id, edges: ResizeEdge) -> bool {
        if self.floating.has_window(&self.space, &window) {
            self.floating
                .interactive_resize_begin(&self.space, window, edges)
        } else {
            self.space.interactive_resize_begin(window, edges)
        }
    }

    pub fn interactive_resize_begin_at(
        &mut self,
        window: W::Id,
        edges: ResizeEdge,
        pos: Point<f64, Logical>,
    ) -> bool {
        if self.floating.has_window(&self.space, &window) {
            self.floating
                .interactive_resize_begin(&self.space, window, edges)
        } else {
            self.space.interactive_resize_begin_at(window, edges, pos)
        }
    }

    pub fn interactive_resize_update(
        &mut self,
        window: &W::Id,
        delta: Point<f64, Logical>,
    ) -> bool {
        if self.floating.has_window(&self.space, window) {
            self.floating
                .interactive_resize_update(&mut self.space, window, delta)
        } else {
            self.space.interactive_resize_update(window, delta)
        }
    }

    pub fn interactive_resize_end(&mut self, window: Option<&W::Id>) {
        if let Some(window) = window {
            if self.floating.has_window(&self.space, window) {
                self.floating.interactive_resize_end(Some(window));
            } else {
                self.space.interactive_resize_end(Some(window));
            }
        } else {
            self.floating.interactive_resize_end(None);
            self.space.interactive_resize_end(None);
        }
    }

    pub fn floating_is_active(&self) -> bool {
        self.floating_is_active.get()
    }

    pub fn floating_logical_to_size_frac(
        &self,
        logical_pos: Point<f64, Logical>,
    ) -> Point<f64, SizeFrac> {
        self.floating
            .logical_to_size_frac(self.space.working_area(), logical_pos)
    }

    pub(super) fn floating_container_allows_splits(&self, id: &W::Id) -> bool {
        self.floating.container_allows_splits(&self.space, id)
    }

    pub(super) fn floating_container_pos(&self, id: &W::Id) -> Option<Point<f64, Logical>> {
        self.floating.container_pos(&self.space, id)
    }

    pub(super) fn move_floating_container_for_window_to(
        &mut self,
        id: &W::Id,
        pos: Point<f64, Logical>,
    ) -> bool {
        self.floating
            .move_container_for_window_to(&mut self.space, id, pos, false)
    }

    pub fn working_area(&self) -> Rectangle<f64, Logical> {
        self.working_area
    }

    pub fn layout_config(&self) -> Option<&tiri_config::LayoutPart> {
        self.layout_config.as_ref()
    }

    #[cfg(test)]
    pub fn tiling(&self) -> &TreeSpace<W> {
        &self.space
    }

    #[cfg(test)]
    pub fn floating(&self) -> FloatingTestView<'_, W> {
        FloatingTestView {
            floating: &self.floating,
            space: &self.space,
        }
    }

    #[cfg(test)]
    pub fn debug_active_floating_wrapper_selected(&self) -> bool {
        self.floating.active_wrapper_selected(&self.space)
    }

    #[cfg(test)]
    pub fn debug_active_floating_container_allows_splits(&self) -> bool {
        self.floating.active_container_allows_splits(&self.space)
    }

    #[cfg(test)]
    pub fn debug_command_context(&self) -> &'static str {
        let target = self.command_target_key();
        if target == self.space.tree().workspace_root() {
            "workspace"
        } else if self.space.tree().is_floating(target) {
            "floating"
        } else {
            "tiling"
        }
    }

    #[cfg(test)]
    pub fn debug_command_target(&self) -> &'static str {
        let target = self.command_target_key();
        if target == self.space.tree().workspace_root() {
            "workspace"
        } else if self.space.tree().is_floating(target) {
            if self.space.tree().is_leaf(target) {
                "floating_window"
            } else {
                "floating_container"
            }
        } else if self.space.tree().is_leaf(target) {
            "tiling_window"
        } else {
            "tiling_container"
        }
    }

    #[cfg(test)]
    pub fn debug_floating_workspace_context(&self) -> bool {
        self.is_floating_workspace_context_active()
    }

    #[cfg(test)]
    pub fn debug_workspace_layout(&self) -> Layout {
        self.space.debug_workspace_layout()
    }

    #[cfg(test)]
    pub fn verify_invariants(&self, move_win_id: Option<&W::Id>) {
        use approx::assert_abs_diff_eq;

        // sway's `ws->fullscreen` is one pointer for the whole workspace.
        let fullscreen = self.fullscreen_window_ids();
        assert!(
            fullscreen.len() <= 1,
            "a workspace has at most one fullscreen window, found {fullscreen:?}"
        );
        let pending_fullscreen: Vec<_> = self
            .windows()
            .filter(|window| window.pending_sizing_mode().is_fullscreen())
            .map(|window| window.id().clone())
            .collect();
        assert!(
            pending_fullscreen.len() <= 1,
            "a workspace cannot request fullscreen for two clients: {pending_fullscreen:?}"
        );
        if let Some(id) = pending_fullscreen.first() {
            assert!(
                self.space.tree().window_owns_fullscreen(id),
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
            &*self.options, &options,
            "options must be base options adjusted for scale"
        );

        assert!(self.view_size.w > 0.);
        assert!(self.view_size.h > 0.);

        assert_eq!(self.background_buffer.size(), self.view_size);
        assert_eq!(
            self.background_buffer.color().components(),
            options.layout.background_color.to_array_unpremul(),
        );

        assert_eq!(self.view_size, self.space.view_size());
        assert_eq!(self.working_area, self.space.parent_area());
        assert_eq!(&self.clock, self.space.clock());
        assert!(Rc::ptr_eq(&self.options, self.space.options()));
        self.space.verify_invariants();

        assert_eq!(self.view_size, self.space.view_size());
        assert_eq!(self.working_area, self.space.working_area());
        assert_eq!(&self.clock, self.space.clock());
        assert!(Rc::ptr_eq(&self.options, self.space.options()));
        self.floating.verify_invariants(&self.space);

        if self.floating.is_empty() {
            assert!(
                !self.floating_is_active.get(),
                "when floating is empty it must never be active"
            );
        } else if self.space.is_empty() {
            assert!(
                self.floating_is_active.get(),
                "when tiling is empty but floating isn't, floating should be active"
            );
        }

        for (tile, tile_pos, visible) in self.tiles_with_render_positions() {
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
    pub(crate) fn layout_tree(&self) -> Option<LayoutTreeNode> {
        if self.floating_is_active.get() {
            self.space.layout_tree_unfocused()
        } else {
            self.space.layout_tree()
        }
    }

    pub(crate) fn floating_layout_tree_nodes(&self) -> Vec<LayoutTreeNode> {
        self.floating.layout_tree_nodes(&self.space)
    }
}

pub(super) fn compute_working_area(output: &Output) -> Rectangle<f64, Logical> {
    layer_map_for_output(output).non_exclusive_zone().to_f64()
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
