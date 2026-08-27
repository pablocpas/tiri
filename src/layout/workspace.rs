use std::cmp::max;
use std::rc::Rc;

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
use tiri_config::{CornerRadius, OutputName, PresetSize, Struts, Workspace as WorkspaceConfig};
use tiri_ipc::{ColumnDisplay, LayoutTreeNode, PositionChange, SizeChange, WindowLayout};

use super::container::{
    Direction, InactiveTilingReference, InsertParentInfo, InteractiveResizeState, Layout, NodeKey,
};
use super::container_tree::{ContainerTree, ContainerTreeRenderElement, RootTilingSubtree};
use super::floating::{
    compute_toplevel_bounds, FloatingResizeResult, FloatingSpace, FloatingSpaceRenderElement,
};
use super::shadow::Shadow;
use super::tile::{Tile, TileRenderSnapshot};
use super::closing_window::ClosingWindow;
use super::{
    ActivateWindow, HitType, InsertPosition, InteractiveResizeData, LayoutCycleEntry,
    LayoutElement, Options, RemovedTile, ResizeHit, ResizeRequest, SizeFrac,
};
use crate::animation::Clock;
use crate::layout::RenderLayer;
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
    /// The workspace-wide container arena and state shared by both sides.
    ///
    /// sway's workspace holds `tiling` and `floating` as two lists over one set of
    /// containers. This is that set, and the floating side asks it for the arena rather than
    /// keeping one — not because either side owns it, but because this is the workspace.
    containers: ContainerTree<W>,

    /// Ongoing interactive resize on the tiled side.
    ///
    /// The floating side runs its own; a window is only ever resized on the side it is on.
    tiling_resize: Option<InteractiveResizeState<W>>,

    /// Tiled windows in their closing animation.
    ///
    /// The floating side keeps its own list because the two are drawn in separate passes, and
    /// that pass order is what puts a closing floating window above a closing tiled one.
    tiling_closing: Vec<ClosingWindow>,

    /// Transient interaction and render state belonging only to the floating side.
    ///
    /// Root membership, geometry and stacking live together in `containers`; keeping them
    /// here as well would create two authorities that every tree mutation had to synchronize.
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
    containers: &'a ContainerTree<W>,
}

#[cfg(test)]
impl<'a, W: LayoutElement> FloatingTestView<'a, W> {
    pub fn tiles(&self) -> impl Iterator<Item = &'a Tile<W>> + 'a {
        self.floating.tiles(self.containers)
    }

    pub fn root_layout_for_window(&self, id: &W::Id) -> Option<Layout> {
        self.floating.root_layout_for_window(self.containers, id)
    }

    pub fn selected_is_container(&self, id: Option<&W::Id>) -> bool {
        self.floating.selected_is_container(self.containers, id)
    }

    pub fn wrapper_selected_for_window(&self, id: &W::Id) -> bool {
        self.floating
            .wrapper_selected_for_window(self.containers, id)
    }

    pub fn is_fullscreen(&self, id: &W::Id) -> bool {
        self.floating.is_fullscreen(self.containers, id)
    }

    pub fn active_window(&self) -> Option<&'a W> {
        self.floating.active_window(self.containers)
    }

    pub fn debug_tree_for_window(&self, id: &W::Id) -> Option<String>
    where
        W::Id: std::fmt::Display,
    {
        self.floating.debug_tree_for_window(self.containers, id)
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
        Tiling = ContainerTreeRenderElement<R>,
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
    /// At this index among the tiling root's children.
    AtRootIndex(usize),
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
        let working_area =
            compute_working_area(&output, options.layout.struts, scale.fractional_scale());

        let containers = ContainerTree::new(
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
            containers,
            tiling_resize: None,
            tiling_closing: Vec::new(),
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
        let working_area = inset_by_struts(
            Rectangle::from_size(view_size),
            options.layout.struts,
            scale.fractional_scale(),
        );

        let containers = ContainerTree::new(
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
            containers,
            tiling_resize: None,
            tiling_closing: Vec::new(),
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
        let workspace_box = self.containers.working_area().size;
        let mut size = Size::from((
            (workspace_box.w * 0.5).floor() as i32,
            (workspace_box.h * 0.75).floor() as i32,
        ));

        // `floating_calculate_constraints` on its automatic settings: a floor of 75 by 50, and
        // as the ceiling the box of the whole output layout. The floor is applied outside the
        // ceiling, as sway applies it, so it wins on an output too small to hold it.
        let view_size = self.containers.view_size();
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
        self.containers.advance_animations();
        self.tiling_closing.retain_mut(|closing| {
            closing.advance_animations();
            closing.are_animations_ongoing()
        });
        self.floating.advance_animations(&mut self.containers);
    }

    pub fn are_animations_ongoing(&self) -> bool {
        self.containers.are_animations_ongoing()
            || !self.tiling_closing.is_empty()
            || self.floating.are_animations_ongoing(&self.containers)
    }

    pub fn are_transitions_ongoing(&self) -> bool {
        self.containers.are_transitions_ongoing()
            || !self.tiling_closing.is_empty()
            || self.floating.are_transitions_ongoing(&self.containers)
    }

    pub fn update_render_elements(&mut self, is_active: bool, layer: RenderLayer) {
        self.containers
            .set_active(is_active, self.floating_is_active.get());
        self.containers.update_render_elements(&self.tiling_resize);

        let view_rect = Rectangle::from_size(self.view_size);
        self.floating
            .update_render_elements(&mut self.containers, is_active, view_rect, layer);

        if layer.is_normal() {
            self.shadow.update_render_elements(
                self.view_size,
                true,
                CornerRadius::default(),
                self.scale.fractional_scale(),
                1.,
            );
        }
    }

    pub fn update_config(&mut self, base_options: Rc<Options>) {
        let scale = self.scale.fractional_scale();
        let options = Rc::new(
            Options::clone(&base_options)
                .with_merged_layout(self.layout_config.as_ref())
                .adjusted_for_scale(scale),
        );

        // Struts are the one layout option that changes the box everything is laid out in, so a
        // reload has to re-derive it rather than pass the box it was given on the last one.
        if options.layout.struts != self.options.layout.struts {
            if let Some(output) = self.output.as_ref() {
                self.working_area = compute_working_area(output, options.layout.struts, scale);
            } else {
                let area = Rectangle::from_size(self.view_size);
                self.working_area = inset_by_struts(area, options.layout.struts, scale);
            }
        }

        self.containers.update_config(
            self.view_size,
            self.working_area,
            self.scale.fractional_scale(),
            options.clone(),
        );

        self.floating.update_config(
            &mut self.containers,
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
        self.containers.update_shaders();
        self.floating.update_shaders(&mut self.containers);
        self.shadow.update_shaders();
    }

    pub fn windows(&self) -> impl Iterator<Item = &W> + '_ {
        self.tiles().map(Tile::window)
    }

    pub fn windows_mut(&mut self) -> impl Iterator<Item = &mut W> + '_ {
        self.tiles_mut().map(Tile::window_mut)
    }

    pub fn tiles(&self) -> impl Iterator<Item = &Tile<W>> + '_ {
        let tiled = self.containers.tiles();
        let floating = self.floating.tiles(&self.containers);
        tiled.chain(floating)
    }

    pub fn tiles_mut(&mut self) -> impl Iterator<Item = &mut Tile<W>> + '_ {
        let tree = self.containers.arena_mut();
        let keys = tree.dfs_leaf_keys();
        tree.tiles_mut_for_keys(&keys)
            .into_iter()
            .map(|(_, tile)| tile)
    }

    pub fn is_floating(&self, id: &W::Id) -> bool {
        self.floating.has_window(&self.containers, id)
    }

    fn is_floating_target(&self, window: Option<&W::Id>) -> bool {
        window.map_or(self.floating_is_active.get(), |id| {
            self.floating.has_window(&self.containers, id)
        })
    }

    /// The one node commands address. Branch and node kind are properties of this key, never
    /// parallel routing state.
    fn command_target_key(&self) -> NodeKey {
        self.containers
            .arena()
            .selected_node_key()
            .unwrap_or_else(|| self.containers.arena().workspace_root())
    }

    pub(super) fn mark_target_key(&self) -> Option<NodeKey> {
        let key = self.command_target_key();
        (key != self.containers.arena().workspace_root()).then_some(key)
    }

    pub(super) fn holds_node(&self, key: NodeKey) -> bool {
        self.containers.holds_node(key)
    }

    pub(super) fn node_has_mark(&self, key: NodeKey, mark: &str) -> bool {
        self.containers.node_has_mark(key, mark)
    }

    pub(super) fn add_mark_to_node(&mut self, key: NodeKey, mark: String) -> bool {
        self.containers.add_mark_to_node(key, mark)
    }

    pub(super) fn remove_mark_from_node(&mut self, key: NodeKey, mark: &str) -> bool {
        self.containers.remove_mark_from_node(key, mark)
    }

    pub(super) fn clear_marks_on_node(&mut self, key: NodeKey) -> bool {
        self.containers.clear_marks_on_node(key)
    }

    pub(super) fn remove_mark_everywhere(&mut self, mark: &str) {
        self.containers.remove_mark_everywhere(mark);
    }

    pub(super) fn clear_marks_everywhere(&mut self) {
        self.containers.clear_marks_everywhere();
    }

    pub(super) fn window_id_with_mark(&self, mark: &str) -> Option<W::Id> {
        self.containers.window_id_with_mark(mark)
    }

    pub(super) fn swap_selected_with_mark(&mut self, mark: &str) -> bool {
        if self.containers.swap_selected_with_mark(mark) {
            self.sync_active_layer_to_command_target();
            return true;
        }
        if !self
            .containers
            .swap_selected_with_mark_at_floating_boundary(mark)
        {
            return false;
        }
        self.floating.forget_resize_that_left(&self.containers);
        self.sync_active_layer_to_command_target();
        true
    }

    /// Keep the render/input layer aligned after a tree mutation changes selection. Selecting
    /// the workspace itself deliberately preserves the last active side; every other node names
    /// its side through ancestry.
    fn sync_active_layer_to_command_target(&mut self) {
        let target = self.command_target_key();
        if target == self.containers.arena().workspace_root() {
            return;
        }
        self.floating_is_active = if self.containers.arena().is_in_floating_branch(target) {
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
        floating: impl FnOnce(&mut FloatingSpace<W>, &mut ContainerTree<W>) -> bool,
        tiled: impl FnOnce(&mut ContainerTree<W>) -> bool,
    ) -> bool {
        self.dispatch_focus_from_workspace(|_| false, floating, tiled)
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
        workspace: impl FnOnce(&mut ContainerTree<W>) -> bool,
        floating: impl FnOnce(&mut FloatingSpace<W>, &mut ContainerTree<W>) -> bool,
        tiled: impl FnOnce(&mut ContainerTree<W>) -> bool,
    ) -> bool {
        let target = self.command_target_key();
        // The workspace's own arm descends into `ws->tiling`, the same side `tiled` acts on,
        // so both take the tiled epilogue. Only the floating arm leaves the layer alone.
        let (moved, landed_in_tiling) = if target == self.containers.arena().workspace_root() {
            (workspace(&mut self.containers), true)
        } else if self.containers.arena().is_in_floating_branch(target) {
            (floating(&mut self.floating, &mut self.containers), false)
        } else {
            (tiled(&mut self.containers), true)
        };
        if landed_in_tiling && moved {
            self.sync_active_layer_to_command_target();
            self.activate_tiling_content();
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
        direction: Direction,
        floating: impl FnOnce(&mut FloatingSpace<W>, &mut ContainerTree<W>),
        tiled: impl FnOnce(&mut ContainerTree<W>) -> bool,
    ) -> bool {
        let target = self.command_target_key();
        if target == self.containers.arena().workspace_root() {
            false
        } else if self.containers.arena().is_in_floating_branch(target) {
            if self.containers.arena().is_floating_root(target) {
                floating(&mut self.floating, &mut self.containers);
            } else {
                let branch = self.containers.arena().branch_root(target);
                let _ = self.containers.move_in_branch(branch, direction);
            }
            true
        } else {
            tiled(&mut self.containers)
        }
    }

    /// Route by the active layer only, ignoring workspace focus elevation: these commands
    /// resolve their own target inside the layer (i3 falls back to the focused leaf).
    fn dispatch_active_layer<R>(
        &mut self,
        floating: impl FnOnce(&mut FloatingSpace<W>, &mut ContainerTree<W>) -> R,
        tiled: impl FnOnce(&mut ContainerTree<W>) -> R,
    ) -> R {
        if self.floating_is_active.get() {
            floating(&mut self.floating, &mut self.containers)
        } else {
            tiled(&mut self.containers)
        }
    }

    /// Route by the layer `window` lives in, defaulting to the active layer for `None`.
    fn dispatch_for_window<R>(
        &mut self,
        window: Option<&W::Id>,
        floating: impl FnOnce(&mut FloatingSpace<W>, &mut ContainerTree<W>) -> R,
        tiled: impl FnOnce(&mut ContainerTree<W>) -> R,
    ) -> R {
        if self.is_floating_target(window) {
            floating(&mut self.floating, &mut self.containers)
        } else {
            tiled(&mut self.containers)
        }
    }

    /// Route one of Tiri's root-ordering commands. These commands address the tiled root list;
    /// directional `move container` commands use `dispatch_move_directional` on both layers.
    fn dispatch_tiling_root_reorder<R: Default>(
        &mut self,
        tiled: impl FnOnce(&mut ContainerTree<W>) -> R,
    ) -> R {
        let target = self.command_target_key();
        if target != self.containers.arena().workspace_root()
            && !self.containers.arena().is_in_floating_branch(target)
        {
            tiled(&mut self.containers)
        } else {
            R::default()
        }
    }

    pub fn focus_mode_toggle_targets_floating(&self) -> bool {
        !self
            .containers
            .arena()
            .is_in_floating_branch(self.command_target_key())
    }

    pub fn current_output(&self) -> Option<&Output> {
        self.output.as_ref()
    }

    pub fn active_window(&self) -> Option<&W> {
        if self.floating_is_active.get() {
            self.floating.active_window(&self.containers)
        } else {
            self.containers.active_window()
        }
    }

    pub fn active_window_mut(&mut self) -> Option<&mut W> {
        if self.floating_is_active.get() {
            self.floating.active_window_mut(&mut self.containers)
        } else {
            self.containers.active_window_mut()
        }
    }

    pub fn active_selection_is_container(&self) -> bool {
        let target = self.command_target_key();
        target != self.containers.arena().workspace_root()
            && !self.containers.arena().is_leaf(target)
    }

    #[cfg(test)]
    pub(super) fn marks_for_window(&self, id: &W::Id) -> Vec<String> {
        self.containers
            .arena()
            .window_key(id)
            .and_then(|key| self.containers.arena().node_marks(key))
            .unwrap_or_default()
            .to_vec()
    }

    pub fn active_command_can_fullscreen(&self) -> bool {
        self.command_target_key() != self.containers.arena().workspace_root()
    }

    pub fn close_window_ids_for_active_selection(&self) -> Vec<W::Id> {
        let target = self.command_target_key();
        if target == self.containers.arena().workspace_root() {
            return self.windows().map(|window| window.id().clone()).collect();
        }

        let ids = if self.containers.arena().is_in_floating_branch(target) {
            self.floating
                .close_window_ids_for_active_selection(&self.containers)
        } else {
            self.containers.close_window_ids_for_active_selection()
        };
        if !ids.is_empty() {
            return ids;
        }

        self.active_window()
            .map(|window| vec![window.id().clone()])
            .unwrap_or_default()
    }

    pub fn is_active_pending_fullscreen(&self) -> bool {
        self.containers.is_active_pending_fullscreen()
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
        let working_area =
            compute_working_area(output, self.options.layout.struts, scale.fractional_scale());
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
            self.containers.update_config(
                size,
                working_area,
                scale.fractional_scale(),
                self.options.clone(),
            );
            self.floating.update_config(
                &mut self.containers,
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

    /// The struts this workspace reserves, which a named workspace may override.
    pub(super) fn struts(&self) -> Struts {
        self.options.layout.struts
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

    #[allow(clippy::too_many_arguments)]
    pub fn add_tile(
        &mut self,
        mut tile: Tile<W>,
        target: WorkspaceAddWindowTarget<W>,
        activate: ActivateWindow,
        is_floating: bool,
        // Unused here. This is upstream's channel for the scrolling layout to animate the tile
        // as it inserts it; `Monitor::move_to_workspace` animates from the render positions it
        // measured either side of the move, which needs nothing from the insert.
        _anim: Option<tiri_config::Animation>,
    ) {
        self.enter_output_for_window(tile.window());
        let floating_active = self.floating_is_active.get();
        let command_target = self.command_target_key();
        let workspace_command_context = command_target == self.containers.arena().workspace_root();
        let map_target = self.containers.arena().view_map_target();
        let map_targets_floating =
            map_target.is_some_and(|target| self.containers.arena().is_in_floating_branch(target));
        // A group root occupies the `ws->floating` slot and is not an insertion parent for a
        // newly mapped normal view. Any node below that boundary is: a leaf, or an inner
        // container created by splitting the root. This is derived from Sway's view-map
        // target, not merely from the currently selected node: selecting the workspace keeps
        // the previous node immediately behind it in the seat order.
        let map_targets_inside_floating_root = map_target.is_some_and(|target| {
            map_targets_floating && !self.containers.arena().is_floating_root(target)
        });
        match target {
            WorkspaceAddWindowTarget::Auto => {
                let has_floating_reinsert_hint = tile.floating_reinsert_hint.is_some();
                // A focused node below the `ws->floating` list boundary is an insertion
                // parent for a newly mapped window. The root entry itself is not. This is a
                // structural distinction: a workspace wrapper can acquire an inner split,
                // and that split accepts a sibling just like one built around a single view.
                // A tile returning from an interactive move carries its original floating
                // parent explicitly and must not be absorbed by whichever group is active now.
                let grouped_floating = !has_floating_reinsert_hint
                    && floating_active
                    && map_targets_inside_floating_root;
                let wants_floating = is_floating || grouped_floating;

                let activate = if !wants_floating && self.containers.has_fullscreen_window() {
                    // Model rule: while a tiling window is fullscreen, newly opened tiling windows
                    // should not steal focus.
                    false
                } else {
                    // Don't steal focus from an active fullscreen window.
                    activate.map_smart(|| !self.is_active_pending_fullscreen())
                };

                if wants_floating {
                    if has_floating_reinsert_hint {
                        self.floating.add_tile_with_restore_hint(
                            &mut self.containers,
                            tile,
                            activate,
                        );
                    } else if grouped_floating {
                        self.floating.add_tile_to_active_container(
                            &mut self.containers,
                            tile,
                            activate,
                        );
                    } else {
                        self.floating.add_tile(&mut self.containers, tile, activate);
                    }

                    if activate || self.containers.is_empty() {
                        self.activate_floating_for_new_content();
                    }
                } else {
                    let tiling_was_empty = self.containers.is_empty();
                    self.containers.add_tile(None, tile, activate);

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
            WorkspaceAddWindowTarget::AtRootIndex(root_index) => {
                let activate = activate.map_smart(|| false);
                self.containers.add_tile(Some(root_index), tile, activate);

                if activate {
                    self.activate_tiling_for_new_content();
                }
            }
            WorkspaceAddWindowTarget::NextTo(next_to) => {
                let floating_has_window = self.floating.has_window(&self.containers, next_to);
                let grouped_floating_target = floating_has_window
                    && self
                        .floating
                        .container_allows_splits(&self.containers, next_to);
                let wants_floating = is_floating || grouped_floating_target;

                let activate = activate
                    .map_smart(|| self.active_window().is_some_and(|win| win.id() == next_to));

                if wants_floating {
                    if grouped_floating_target {
                        self.floating.add_tile_to_container_of(
                            &mut self.containers,
                            next_to,
                            tile,
                            activate,
                        );
                    } else if floating_has_window {
                        self.floating
                            .add_tile_above(&mut self.containers, next_to, tile, activate);
                    } else {
                        self.center_new_floating_tile_on(&mut tile, next_to);
                        self.floating.add_tile(&mut self.containers, tile, activate);
                    }

                    if activate || self.containers.is_empty() {
                        self.activate_floating_for_new_content();
                    }
                } else if floating_has_window {
                    self.containers.add_tile(None, tile, activate);

                    if activate {
                        self.activate_tiling_for_new_content();
                    }
                } else {
                    if self
                        .containers
                        .tiles()
                        .any(|tile| tile.window().id() == next_to)
                    {
                        self.containers.add_tile_right_of(next_to, tile, activate);
                    } else {
                        error!("next_to target disappeared while placing a new tiled window");
                        self.containers.add_tile(None, tile, activate);
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

        // The workspace's fullscreen pointer is one for the whole workspace, so an arriving
        // window that still requests fullscreen has to be reconciled against whatever is
        // already holding it. The tiled inserts do this inside the tree; the floating ones
        // never did, which let two floating clients sit pending fullscreen side by side. sway
        // resolves the same collision in `container_handle_fullscreen_reparent`.
        self.containers.sync_fullscreen_window();
    }

    /// Place a new floating tile centred over the tiled window it belongs to.
    ///
    /// Think a dialog opening on top of its parent.
    fn center_new_floating_tile_on(&self, tile: &mut Tile<W>, next_to: &W::Id) {
        let Some((next_to_tile, render_pos, _visible)) = self
            .containers
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
                .clamp_within_working_area(self.containers.working_area(), pos, tile_size);
        let pos = self
            .floating
            .logical_to_size_frac(self.containers.working_area(), pos);
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
        self.containers
            .add_tile_to_root_container(root_idx, tile_idx, tile, activate);

        if activate {
            self.floating_is_active = FloatingActive::No;
            self.activate_tiling_content();
        }
    }

    pub(super) fn tiling_insert_parent_info(&self, window: &W::Id) -> Option<InsertParentInfo> {
        self.containers.insert_parent_info_for_window(window)
    }

    fn inactive_tiling_reference(&self) -> Option<InactiveTilingReference> {
        self.containers.inactive_tiling_reference()
    }

    fn activate_tiling_content(&mut self) {
        self.containers.clear_workspace_selection();
    }

    fn focus_tiling_key(&mut self, key: super::container::NodeKey) -> bool {
        let focused = self.containers.focus_inactive_tiling_key(key);
        if focused {
            self.floating_is_active = FloatingActive::No;
            self.activate_tiling_content();
        }
        focused
    }

    fn window_has_fullscreen_focus_scope(&self, window: &W) -> bool {
        self.containers.is_fullscreen(window)
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

        self.containers
            .window_for_inactive_tiling_key(key)
            .is_some_and(|window| self.window_has_fullscreen_focus_scope(window))
    }

    pub(super) fn focus_floating_window(&mut self, id: &W::Id, raise: bool) -> bool {
        let focused = if raise {
            self.floating.activate_window(&mut self.containers, id)
        } else {
            self.floating
                .activate_window_without_raising(&mut self.containers, id)
        };
        if focused {
            self.floating_is_active = FloatingActive::Yes;
        }
        focused
    }

    pub(super) fn restore_inactive_floating(&mut self) -> bool {
        let Some(id) = self.containers.inactive_floating_window_id() else {
            return false;
        };
        self.focus_floating_window(&id, false)
    }

    pub(super) fn restore_inactive_tiling(&mut self) -> Option<bool> {
        let key = self.containers.inactive_tiling_key()?;
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
        if !self.containers.is_tiling_leaf(target) {
            return Err(tile);
        }
        let Some(displaced) = self.containers.replace_tiling_tile(target, tile) else {
            // is_tiling_leaf just said otherwise; the tile is already gone into the tree.
            return Ok(());
        };
        self.containers
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

        let inserted = self
            .containers
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

        let inserted = self
            .containers
            .insert_tile_split_root(direction, tile, activate);

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

        self.containers
            .add_root_tiling_subtree(None, subtree, activate);

        if activate {
            self.floating_is_active = FloatingActive::No;
            self.activate_tiling_content();
        }
    }

    fn update_focus_floating_tiling_after_removing(&mut self, removed_from_floating: bool) {
        // An elevation can only belong to the active layer (the inactive layer's elevation is
        // already dropped by construction), so clear it when that active layer empties out.
        if self.containers.is_empty() {
            self.containers.clear_selection_context();
        }

        if removed_from_floating {
            if self.floating.is_empty(&self.containers) {
                self.floating_is_active = FloatingActive::No;
                self.activate_tiling_content();
            }
        } else {
            // Tiling should remain focused if both are empty.
            if self.containers.is_empty() && !self.floating.is_empty(&self.containers) {
                self.floating_is_active = FloatingActive::Yes;
            }
        }
    }

    pub fn remove_tile(&mut self, id: &W::Id, transaction: Transaction) -> RemovedTile<W> {
        let mut from_floating = false;
        let removed = if self.floating.has_window(&self.containers, id) {
            from_floating = true;
            self.floating.remove_tile(&mut self.containers, id)
        } else {
            self.containers.remove_tile(id, transaction)
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
            self.floating.remove_active_tile(&mut self.containers)?
        } else {
            self.containers.remove_active_tile(transaction)?
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

        let subtree = self.containers.remove_active_root_tiling_subtree()?;

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

        let subtree = self.containers.remove_active_tiling_subtree()?;

        if let Some(output) = &self.output {
            for tile in subtree.tiles() {
                tile.window().output_leave(output);
            }
        }

        self.update_focus_floating_tiling_after_removing(from_floating);

        Some(subtree)
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
                .new_window_size(&self.containers, width, height, rules)
        } else {
            self.containers.new_window_size(width, height, rules)
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
                state.bounds = Some(
                    self.floating
                        .new_window_toplevel_bounds(&self.containers, rules),
                );
            } else {
                state.bounds = Some(self.containers.new_window_toplevel_bounds(rules));
            }
        });
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
        self.containers.focus_root_container(index);
        self.activate_tiling_content();
    }

    pub fn focus_leaf_in_root_container(&mut self, index: u8) {
        if self.floating_is_active.get() {
            return;
        }
        self.containers.focus_leaf_in_root_container(index);
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
        if self.containers.has_fullscreen_window() {
            // Fullscreen workspace targets resolve to the inactive focus under
            // the fullscreen subtree. Keep tiling active as-is.
            self.floating_is_active = FloatingActive::No;
            self.activate_tiling_content();
            return true;
        }

        let Some((root_layout, child_count)) = self.containers.root_layout_and_child_count() else {
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
            Direction::Left | Direction::Up => self.containers.focus_root_container_last(),
            Direction::Right | Direction::Down => self.containers.focus_root_container_first(),
        }
        self.floating_is_active = FloatingActive::No;
        self.activate_tiling_content();
        true
    }

    pub(super) fn has_tiling_windows(&self) -> bool {
        !self.containers.is_empty()
    }

    pub(super) fn focus_workspace_node(&mut self) {
        if self.floating.is_empty(&self.containers) {
            self.floating_is_active = FloatingActive::No;
        } else {
            self.floating_is_active = FloatingActive::Yes;
        }
        let _ = self.containers.select_root_container();
    }

    fn focus_is_elevated(&self) -> bool {
        self.containers.workspace_is_selected()
    }

    /// Switch the active layer to tiling, with the new window as what commands are aimed at.
    ///
    /// Measured against sway 1.11: `focus parent` selects the workspace, but opening a
    /// window ends that — the next command goes to the window. The elevation records that
    /// the user asked for the workspace at some point; a window taking focus answers the
    /// question.
    fn activate_tiling_for_new_content(&mut self) {
        self.floating_is_active = FloatingActive::No;
        self.containers.clear_workspace_selection();
    }

    fn activate_floating_for_new_content(&mut self) {
        self.floating_is_active = FloatingActive::Yes;
        self.containers.clear_workspace_selection();
    }

    #[cfg(test)]
    pub(super) fn is_floating_workspace_context_active(&self) -> bool {
        self.floating_is_active.get() && self.focus_is_elevated()
    }

    /// Whether the workspace's real node is selected.
    #[cfg(test)]
    pub(super) fn tiling_targets_workspace(&self) -> bool {
        self.containers.workspace_is_selected()
    }

    #[cfg(test)]
    pub(super) fn is_tiling_workspace_context_active(&self) -> bool {
        !self.floating_is_active.get() && self.tiling_targets_workspace()
    }

    pub fn focus_window_by_id(&mut self, id: &W::Id) -> bool {
        if self.floating.has_window(&self.containers, id)
            && self.floating.focus_window_by_id(&mut self.containers, id)
        {
            self.floating_is_active = FloatingActive::Yes;
            return true;
        }

        if self.containers.activate_window(id) {
            self.floating_is_active = FloatingActive::No;
            self.activate_tiling_content();
            return true;
        }

        false
    }

    pub fn move_left(&mut self) -> bool {
        self.dispatch_move_directional(
            Direction::Left,
            |f, tree| f.move_left(tree),
            |t| t.move_left(),
        )
    }

    pub fn move_right(&mut self) -> bool {
        self.dispatch_move_directional(
            Direction::Right,
            |f, tree| f.move_right(tree),
            |t| t.move_right(),
        )
    }

    pub fn move_container_left(&mut self) -> bool {
        self.move_left()
    }

    pub fn move_container_right(&mut self) -> bool {
        self.move_right()
    }

    pub fn move_container_to_first(&mut self) {
        self.dispatch_tiling_root_reorder(|t| t.move_root_container_to_first())
    }

    pub fn move_column_to_first(&mut self) {
        self.move_container_to_first();
    }

    pub fn move_container_to_last(&mut self) {
        self.dispatch_tiling_root_reorder(|t| t.move_root_container_to_last())
    }

    pub fn move_column_to_last(&mut self) {
        self.move_container_to_last();
    }

    pub fn move_container_to_index(&mut self, index: usize) {
        self.dispatch_tiling_root_reorder(|t| t.move_root_container_to_index(index))
    }

    pub fn move_column_to_index(&mut self, index: usize) {
        self.move_container_to_index(index);
    }

    pub fn move_down(&mut self) -> bool {
        self.dispatch_move_directional(
            Direction::Down,
            |f, tree| f.move_down(tree),
            |t| t.move_down(),
        )
    }

    pub fn move_up(&mut self) -> bool {
        self.dispatch_move_directional(Direction::Up, |f, tree| f.move_up(tree), |t| t.move_up())
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

    /// sway's `swap container with id|con_id|mark <arg>`, addressed by window.
    ///
    /// One arena holds both sides of the workspace, so this needs no routing: the tree
    /// answers for the selected node wherever it sits, and refuses the pairs it cannot
    /// honour.
    pub fn swap_window_with(&mut self, target: &W::Id) -> bool {
        if self.containers.swap_selected_with_window(target) {
            self.sync_active_layer_to_command_target();
            return true;
        }
        if !self
            .containers
            .swap_selected_with_window_at_floating_boundary(target)
        {
            return false;
        }
        self.floating.forget_resize_that_left(&self.containers);
        self.sync_active_layer_to_command_target();
        true
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

    /// sway's `move position center`, which is a floating-layer command.
    ///
    /// A tiled window has no position to set — it fills the slot its container gives it — so
    /// the tiling half is inert rather than inventing a meaning for centering inside a tree.
    /// sway says the same thing by refusing `move position` on a tiled container.
    pub fn center_window(&mut self, id: Option<&W::Id>) {
        self.dispatch_for_window(
            id,
            |f, tree| f.center_window(tree, id),
            |t| t.center_window(id),
        );
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

    /// Route one semantic resize request by the addressed node, not merely by layer.
    pub fn resize_window(&mut self, window: Option<&W::Id>, request: ResizeRequest) {
        let target = match window {
            Some(id) => self.containers.arena().window_key(id),
            None => Some(self.command_target_key()),
        };
        let Some(target) = target else {
            return;
        };
        if target == self.containers.arena().workspace_root() {
            // `focus parent` can elevate the seat to the workspace. sway then has no
            // handler-context container for `resize`; it must not fall back to the last
            // active floating root merely because that layer remains active for rendering.
            return;
        }

        if self.containers.arena().is_floating_root(target) {
            self.floating
                .resize_window(&mut self.containers, window, request);
        } else {
            // A descendant of a floating root is tiled inside that root. sway takes the
            // geometric floating path only for the root itself.
            self.containers.resize_node(target, request);
        }
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

    /// sway's `focus next|prev [sibling]` inside whichever tree branch owns the selection.
    pub fn focus_along_parent(&mut self, forward: bool, descend: bool) -> bool {
        let target = self.command_target_key();
        if target == self.containers.arena().workspace_root() {
            return false;
        }
        if self.containers.arena().is_in_floating_branch(target) {
            self.floating
                .focus_along_parent(&mut self.containers, forward, descend)
        } else {
            self.containers.focus_along_parent(forward, descend)
        }
    }

    pub fn focus_parent(&mut self) {
        let target = self.command_target_key();
        if target == self.containers.arena().workspace_root() {
            return;
        }
        if self.containers.arena().is_in_floating_branch(target) {
            let _ = self.floating.focus_parent(&mut self.containers);
        } else {
            let _ = self.containers.focus_parent();
        }
    }

    pub fn focus_child(&mut self) {
        let _ = self.dispatch_focus_from_workspace(
            // sway descends through the workspace's active tiling child
            // (`seat_get_active_tiling_child`). Floating roots live in `ws->floating` and
            // are not candidates from workspace focus, which is why this lands in the tiled
            // side and takes the tiled epilogue with it.
            |tree| !tree.is_empty() && tree.focus_child(),
            |floating, tree| floating.focus_child(tree),
            |tree| tree.focus_child(),
        );
    }

    /// Route a split/layout command: workspace-level targets apply to the workspace layout,
    /// floating targets drop focus elevation unless the selected floating workspace container
    /// preserves it. Model rule: split/layout commands work in both floating and tiled branches.
    fn dispatch_layout(
        &mut self,
        workspace: impl FnOnce(&mut ContainerTree<W>),
        tiled: impl FnOnce(&mut ContainerTree<W>),
        floating: impl FnOnce(&mut FloatingSpace<W>, &mut ContainerTree<W>),
    ) {
        let target = self.command_target_key();
        if target == self.containers.arena().workspace_root() {
            workspace(&mut self.containers);
        } else if self.containers.arena().is_in_floating_branch(target) {
            floating(&mut self.floating, &mut self.containers);
        } else {
            tiled(&mut self.containers);
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
        let parent_is_vertical =
            self.containers.command_target_parent_layout() == Some(Layout::SplitV);
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
        self.containers
            .arena()
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

        if self.floating.has_window(&self.containers, window) {
            self.floating
                .set_fullscreen(&mut self.containers, window, is_fullscreen);
            return;
        }

        self.containers.set_fullscreen(window, is_fullscreen);
    }

    pub fn toggle_fullscreen(&mut self, window: &W::Id) {
        if self.floating.has_window(&self.containers, window) {
            let current = self.floating.is_fullscreen(&self.containers, window);
            self.set_fullscreen(window, !current);
            return;
        }

        let tile = self
            .tiles()
            .find(|tile| tile.window().id() == window)
            .unwrap();
        // Use space.is_fullscreen() as the source of truth instead of pending_sizing_mode(),
        // which updates asynchronously after animations complete.
        let current = self.containers.is_fullscreen(tile.window());
        self.set_fullscreen(window, !current);
    }

    pub fn toggle_fullscreen_for_command(&mut self, _window: &W::Id) {
        // Resolve once to the tree object the command addresses. Floating and tiling are two
        // workspace branches, not two fullscreen semantics: a leaf owns client fullscreen,
        // while a container owns compositor-side fullscreen as the same stable NodeKey.
        let target = self.command_target_key();

        if target == self.containers.arena().workspace_root() {
            return;
        }
        if self.containers.arena().is_leaf(target) {
            let Some(id) = self
                .containers
                .arena()
                .get_tile(target)
                .map(|tile| tile.window().id().clone())
            else {
                return;
            };
            self.toggle_fullscreen(&id);
        } else {
            if self.containers.arena().fullscreen_key() != Some(target) {
                // A previous leaf has protocol state to revoke. A previous container does
                // not; replacing the workspace pointer below is sufficient for it.
                if let Some(id) = self.containers.arena().fullscreen_leaf_window_id().cloned() {
                    self.set_fullscreen(&id, false);
                }
            }
            self.containers.toggle_fullscreen_container(target);
        }
    }

    pub fn set_windowed_fullscreen(&mut self, window: &W::Id, is_fullscreen: bool) {
        self.set_fullscreen(window, is_fullscreen);
    }

    pub fn toggle_window_floating(&mut self, id: Option<&W::Id>) {
        let Some(transfer) = self.resolve_floating_transfer(id) else {
            return;
        };
        let id = transfer.window_id().clone();

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
        let command_targets_workspace = command_target == self.containers.arena().workspace_root();
        let command_targets_tiling = !command_targets_workspace
            && !self
                .containers
                .arena()
                .is_in_floating_branch(command_target);

        if requested_id.is_none() && command_targets_workspace {
            // A selected workspace with no tiled children has nothing that can become floating.
            if self.containers.is_empty() {
                return None;
            }
        }

        let explicit_window = requested_id.is_some();
        let active_id = self.active_window().map(|win| win.id().clone());
        let target_is_active = requested_id.is_none_or(|id| Some(id) == active_id.as_ref());
        let id = requested_id.cloned().or(active_id)?;
        let is_floating = self.floating.has_window(&self.containers, &id);
        let inactive_tiling_reference = if is_floating {
            self.inactive_tiling_reference()
        } else {
            None
        };

        if !explicit_window
            && target_is_active
            && command_targets_workspace
            && !self.containers.is_empty()
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
            && self.containers.selected_is_container()
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
                    .active_container_is_workspace_floated(&self.containers);
                let tiling_was_empty = self.containers.is_empty();
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
            FloatTransfer::Workspace { focus_id: _ } => {
                let Some((subtree, rect)) = self.containers.take_workspace_subtree_for_floating()
                else {
                    return false;
                };
                if !self
                    .floating
                    .add_subtree(&mut self.containers, subtree, rect, true)
                {
                    return false;
                }
                self.floating_is_active = FloatingActive::Yes;
                false
            }
            FloatTransfer::SelectedContainer { focus_id: _ } => {
                let Some((subtree, rect)) = self.containers.take_selected_subtree() else {
                    return false;
                };
                if !self
                    .floating
                    .add_subtree(&mut self.containers, subtree, rect, false)
                {
                    return false;
                }
                self.floating_is_active = FloatingActive::Yes;
                false
            }
            FloatTransfer::Window {
                id,
                target_is_active,
            } => {
                let Some((subtree, rect)) = self.containers.subtree_for_window_floating(&id) else {
                    return false;
                };
                if let Some(tile) = self.containers.arena_mut().get_tile_mut(subtree) {
                    tile.stop_move_animations();
                }

                if !self
                    .floating
                    .add_subtree(&mut self.containers, subtree, rect, false)
                {
                    return false;
                }
                // The floating side takes over either because the window that moved was the
                // active one, or because floating it emptied the tiled side and left nothing
                // for the active layer to point at.
                if target_is_active || self.containers.is_empty() {
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
                    &mut self.containers,
                    &id,
                    tiling_reference.as_ref(),
                    was_workspace && tiling_was_empty,
                ) {
                    if target_is_active {
                        self.finish_active_unfloat();
                    }
                    return false;
                }

                let unfloated = self.floating.unfloat_window(
                    &mut self.containers,
                    &id,
                    tiling_reference.as_ref(),
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
                    &mut self.containers,
                    &id,
                    tiling_reference.as_ref(),
                );
                if unfloated && target_is_active {
                    self.finish_active_unfloat();
                }
                unfloated
            }
        }
    }

    fn finish_active_unfloat(&mut self) {
        self.floating_is_active = FloatingActive::No;
        self.activate_tiling_content();
    }

    pub fn scratchpad_window_id(&self) -> Option<W::Id> {
        self.floating
            .tiles(&self.containers)
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
    pub fn take_tile_for_hiding_in_scratchpad(&mut self, id: &W::Id) -> Option<Tile<W>> {
        self.set_fullscreen(id, false);
        self.take_tile_for_scratchpad(id)
    }

    /// `container_fullscreen_disable` on whatever this workspace's one fullscreen pointer
    /// names, leaf or container.
    pub fn clear_fullscreen(&mut self) {
        let Some(scope) = self.containers.arena().fullscreen_key() else {
            return;
        };

        if self.containers.arena().is_leaf(scope) {
            if let Some(id) = self.containers.arena().fullscreen_leaf_window_id().cloned() {
                self.set_fullscreen(&id, false);
            }
            return;
        }

        self.containers.toggle_fullscreen_container(scope);
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
            tile.floating_pos = None;

            let size = self.scratchpad_default_size(&tile);
            tile.floating_window_size = Some(size);
            tile.window_mut().request_size_once(size, false);

            let working_area = self.containers.working_area();
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
        self.floating.add_tile(&mut self.containers, tile, activate);

        if activate || self.containers.is_empty() {
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
        if self.floating.is_empty(&self.containers) || self.containers.is_empty() {
            return;
        }

        self.containers.clear_selection_context();
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
        self.containers.clear_selection_context();
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
                .move_window(&mut self.containers, id, x, y, animate);
        } else {
            // If the target tile isn't floating, set its stored floating position.
            let working_area = self.containers.working_area();
            let tile = if let Some(id) = id {
                self.containers
                    .tiles_mut()
                    .find(|tile| tile.window().id() == id)
                    .unwrap()
            } else if let Some(tile) = self.containers.active_tile_mut() {
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
        let fullscreen_scope = self.floating.fullscreen_key(&self.containers);
        let tiled = self
            .containers
            .tiles_with_render_positions()
            .map(move |(tile, pos, visible)| (tile, pos, visible && fullscreen_scope.is_none()));

        let floating = self.floating.tiles_with_render_positions(&self.containers);
        let visible = self.is_floating_visible();
        let tree = self.containers.arena();
        let floating = floating.map(move |(tile, pos)| {
            let in_scope =
                fullscreen_scope.is_none_or(|scope| tree.is_descendant(tile.node_key(), scope));
            (tile, pos, visible && in_scope)
        });

        floating.chain(tiled)
    }

    pub fn tiles_with_render_positions_mut(
        &mut self,
        round: bool,
    ) -> impl Iterator<Item = (&mut Tile<W>, Point<f64, Logical>)> {
        let scale = self.scale.fractional_scale();
        let layouts: Vec<_> = self
            .containers
            .arena()
            .leaf_layouts()
            .iter()
            .map(|info| (info.key, info.rect.loc))
            .collect();
        let keys: Vec<_> = layouts.iter().map(|(key, _)| *key).collect();
        let locs: Vec<_> = layouts.iter().map(|(_, loc)| *loc).collect();
        self.containers
            .arena_mut()
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
        let tiled = self.containers.tiles_with_ipc_layouts();
        let floating = self.floating.tiles_with_ipc_layouts(&self.containers);
        floating.chain(tiled)
    }

    pub fn active_window_visual_rectangle(&self) -> Option<Rectangle<f64, Logical>> {
        if self.floating_is_active.get() {
            self.floating
                .active_window_visual_rectangle(&self.containers)
        } else {
            self.containers.active_tile_visual_rectangle()
        }
    }

    pub fn popup_target_rect(&self, window: &W::Id) -> Option<Rectangle<f64, Logical>> {
        if self.floating.has_window(&self.containers, window) {
            self.floating.popup_target_rect(&self.containers, window)
        } else {
            self.containers.popup_target_rect(window)
        }
    }

    pub fn render_tiling<R: NiriRenderer>(
        &self,
        ctx: RenderCtx<R>,
        xray_pos: XrayPos,
        focus_ring: bool,
        layer: RenderLayer,
        push: &mut dyn FnMut(WorkspaceRenderElement<R>),
    ) {
        if self.floating.fullscreen_key(&self.containers).is_some() {
            return;
        }
        let tiling_focus_ring = focus_ring && !self.floating_is_active();
        self.containers.render(
            &self.tiling_closing,
            ctx,
            xray_pos,
            tiling_focus_ring,
            layer,
            &mut |elem| push(elem.into()),
        );
    }

    pub fn render_tiling_as_offscreen<R: NiriRenderer>(
        &self,
        renderer: &mut GlesRenderer,
        target: RenderTarget,
        focus_ring: bool,
        push: &mut dyn FnMut(WorkspaceRenderElement<R>),
    ) {
        if self.floating.fullscreen_key(&self.containers).is_some() {
            return;
        }
        let tiling_focus_ring = focus_ring && !self.floating_is_active();
        if let Some(elem) =
            self.containers
                .render_as_offscreen(&self.tiling_closing, renderer, target, tiling_focus_ring)
        {
            push(elem.into());
        }
    }

    pub fn render_floating<R: NiriRenderer>(
        &self,
        ctx: RenderCtx<R>,
        xray_pos: XrayPos,
        focus_ring: bool,
        layer: RenderLayer,
        push: &mut dyn FnMut(WorkspaceRenderElement<R>),
    ) {
        if !self.is_floating_visible() && layer.is_normal() {
            return;
        }

        let view_rect = Rectangle::from_size(self.view_size);
        let floating_focus_ring = focus_ring && self.floating_is_active();
        self.floating.render(
            &self.containers,
            ctx,
            xray_pos,
            view_rect,
            floating_focus_ring,
            layer,
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
        self.containers.render_above_top_layer()
            || self
                .floating
                .fullscreen_key(&self.containers)
                .is_some_and(|key| {
                    self.containers.arena().container_info(key).is_some()
                        || self
                            .containers
                            .arena()
                            .get_tile(key)
                            .is_some_and(|tile| tile.window().sizing_mode().is_fullscreen())
                })
    }

    pub fn is_floating_visible(&self) -> bool {
        // If the focus is on a fullscreen tiling window, hide the floating windows.
        matches!(
            self.floating_is_active,
            FloatingActive::Yes | FloatingActive::NoButRaised
        ) || self.floating.fullscreen_key(&self.containers).is_some()
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
        if self.floating.has_window(&self.containers, window) {
            self.floating.start_close_animation_for_window(
                &mut self.containers,
                renderer,
                window,
                blocker,
            );
        } else {
            self.containers.start_close_animation_for_window(
                &mut self.tiling_closing,
                renderer,
                window,
                blocker,
            );
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
            &self.containers,
            renderer,
            snapshot,
            tile_size,
            tile_pos,
            blocker,
        );
    }

    pub fn start_open_animation(&mut self, id: &W::Id) -> bool {
        self.containers.start_open_animation(id)
            || self.floating.start_open_animation(&mut self.containers, id)
    }

    pub fn window_under(&self, pos: Point<f64, Logical>) -> Option<(&W, HitType)> {
        if self.is_floating_visible() {
            if let Some(rv) = self.floating.window_under(&self.containers, pos) {
                return Some(rv);
            }
        }

        self.containers.window_under(pos)
    }

    pub fn resize_edges_under(&mut self, pos: Point<f64, Logical>) -> Option<ResizeEdge> {
        self.resize_hit_under(pos).map(|hit| hit.edges)
    }

    pub fn resize_hit_under(&mut self, pos: Point<f64, Logical>) -> Option<ResizeHit<W::Id>> {
        if self.is_active_pending_fullscreen() {
            return None;
        }

        if self.is_floating_visible() {
            match self.floating.resize_hit_under(&self.containers, pos) {
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

        self.containers.resize_hit_under(pos)
    }

    pub fn descendants_added(&mut self, id: &W::Id) -> bool {
        self.floating.descendants_added(&mut self.containers, id)
    }

    pub fn update_window(&mut self, window: &W::Id, serial: Option<Serial>) {
        if !self
            .floating
            .update_window(&mut self.containers, window, serial)
        {
            self.containers.update_window(window, serial);
        }
    }

    pub fn refresh(&mut self, is_active: bool, is_focused: bool) {
        let _span = tracy_client::span!("Workspace::refresh");
        self.containers.refresh(
            &self.tiling_resize,
            is_active && !self.floating_is_active.get(),
            is_focused,
        );
        self.floating.refresh(
            &mut self.containers,
            is_active && self.floating_is_active.get(),
            is_focused,
        );
    }

    pub fn is_urgent(&self) -> bool {
        self.windows().any(|win| win.is_urgent())
    }

    pub fn activate_window(&mut self, window: &W::Id) -> bool {
        if self.floating.activate_window(&mut self.containers, window) {
            self.floating_is_active = FloatingActive::Yes;
            true
        } else if self.containers.activate_window(window) {
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
            .activate_window_without_raising(&mut self.containers, window)
        {
            self.floating_is_active = FloatingActive::Yes;
            true
        } else if self.containers.activate_window(window) {
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
        self.containers.insert_position(pos)
    }

    pub(super) fn insert_hint_area(
        &self,
        position: &InsertPosition,
    ) -> Option<Rectangle<f64, Logical>> {
        self.containers.insert_hint_area(position)
    }

    pub fn interactive_resize_begin(&mut self, window: W::Id, edges: ResizeEdge) -> bool {
        if self.floating.has_window(&self.containers, &window) {
            self.floating
                .interactive_resize_begin(&self.containers, window, edges)
        } else {
            self.containers
                .interactive_resize_begin(&mut self.tiling_resize, window, edges)
        }
    }

    pub fn interactive_resize_begin_at(
        &mut self,
        window: W::Id,
        edges: ResizeEdge,
        pos: Point<f64, Logical>,
    ) -> bool {
        if self.floating.has_window(&self.containers, &window) {
            self.floating
                .interactive_resize_begin(&self.containers, window, edges)
        } else {
            self.containers
                .interactive_resize_begin_at(&mut self.tiling_resize, window, edges, pos)
        }
    }

    pub fn interactive_resize_update(
        &mut self,
        window: &W::Id,
        delta: Point<f64, Logical>,
    ) -> bool {
        if self.floating.has_window(&self.containers, window) {
            self.floating
                .interactive_resize_update(&mut self.containers, window, delta)
        } else {
            self.containers
                .interactive_resize_update(&self.tiling_resize, window, delta)
        }
    }

    pub fn interactive_resize_end(&mut self, window: Option<&W::Id>) {
        if let Some(window) = window {
            if self.floating.has_window(&self.containers, window) {
                self.floating.interactive_resize_end(Some(window));
            } else {
                self.containers
                    .interactive_resize_end(&mut self.tiling_resize, Some(window));
            }
        } else {
            self.floating.interactive_resize_end(None);
            self.containers
                .interactive_resize_end(&mut self.tiling_resize, None);
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
            .logical_to_size_frac(self.containers.working_area(), logical_pos)
    }

    pub(super) fn floating_container_allows_splits(&self, id: &W::Id) -> bool {
        self.floating.container_allows_splits(&self.containers, id)
    }

    pub(super) fn floating_container_pos(&self, id: &W::Id) -> Option<Point<f64, Logical>> {
        self.floating.container_pos(&self.containers, id)
    }

    pub(super) fn move_floating_container_for_window_to(
        &mut self,
        id: &W::Id,
        pos: Point<f64, Logical>,
    ) -> bool {
        self.floating
            .move_container_for_window_to(&mut self.containers, id, pos, false)
    }

    pub fn working_area(&self) -> Rectangle<f64, Logical> {
        self.working_area
    }

    pub fn layout_config(&self) -> Option<&tiri_config::LayoutPart> {
        self.layout_config.as_ref()
    }

    #[cfg(test)]
    pub fn container_tree(&self) -> &ContainerTree<W> {
        &self.containers
    }

    #[cfg(test)]
    pub fn floating(&self) -> FloatingTestView<'_, W> {
        FloatingTestView {
            floating: &self.floating,
            containers: &self.containers,
        }
    }

    #[cfg(test)]
    pub fn debug_active_floating_wrapper_selected(&self) -> bool {
        self.floating.active_wrapper_selected(&self.containers)
    }

    #[cfg(test)]
    pub fn debug_command_context(&self) -> &'static str {
        let target = self.command_target_key();
        if target == self.containers.arena().workspace_root() {
            "workspace"
        } else if self.containers.arena().is_in_floating_branch(target) {
            "floating"
        } else {
            "tiling"
        }
    }

    #[cfg(test)]
    pub fn debug_command_target(&self) -> &'static str {
        let target = self.command_target_key();
        if target == self.containers.arena().workspace_root() {
            "workspace"
        } else if self.containers.arena().is_in_floating_branch(target) {
            if self.containers.arena().is_leaf(target) {
                "floating_window"
            } else {
                "floating_container"
            }
        } else if self.containers.arena().is_leaf(target) {
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
        self.containers.debug_workspace_layout()
    }

    /// True while a configure this workspace sent is still unanswered.
    pub fn has_pending_layouts(&self) -> bool {
        self.containers.has_pending_layouts()
    }

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
                self.containers.arena().window_owns_fullscreen(id),
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

        assert_eq!(self.view_size, self.containers.view_size());
        assert_eq!(self.working_area, self.containers.parent_area());
        assert_eq!(&self.clock, self.containers.clock());
        assert!(Rc::ptr_eq(&self.options, self.containers.options()));
        self.containers.verify_invariants();

        assert_eq!(self.view_size, self.containers.view_size());
        assert_eq!(self.working_area, self.containers.working_area());
        assert_eq!(&self.clock, self.containers.clock());
        assert!(Rc::ptr_eq(&self.options, self.containers.options()));
        self.floating.verify_invariants(&self.containers);

        if self.floating.is_empty(&self.containers) {
            assert!(
                !self.floating_is_active.get(),
                "when floating is empty it must never be active"
            );
        } else if self.containers.is_empty() {
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
            self.containers.layout_tree_unfocused()
        } else {
            self.containers.layout_tree()
        }
    }

    pub(crate) fn floating_layout_tree_nodes(&self) -> Vec<LayoutTreeNode> {
        self.floating.layout_tree_nodes(&self.containers)
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
