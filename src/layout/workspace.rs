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

use super::container::{
    DetachedNode, Direction, InactiveTilingReference, InsertParentInfo, Layout,
};
use super::floating::{
    compute_toplevel_bounds, FloatingResizeResult, FloatingSpace, FloatingSpaceRenderElement,
};
use super::legacy_column::{Column, ColumnWidth};
use super::shadow::Shadow;
use super::tile::{Tile, TileRenderSnapshot};
use super::tiling::{RootTilingSubtree, TilingSpace, TilingSpaceRenderElement};
use super::{
    ActivateWindow, HitType, InsertPosition, InteractiveResizeData, LayoutElement, Options,
    RemovedTile, ResizeHit, SizeFrac,
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
    /// The i3/sway tiling layout.
    tiling: TilingSpace<W>,

    /// The floating layout.
    floating: FloatingSpace<W>,

    /// Whether the floating layout is active instead of the tiling layout.
    floating_is_active: FloatingActive,

    /// Whether command focus rests on concrete content or is elevated to the workspace.
    ///
    /// The active layer is tracked separately by `floating_is_active`; combined they describe
    /// the full command context (e.g. elevated + floating-active = "floating workspace
    /// context"). See [`WorkspaceFocus`].
    workspace_focus: WorkspaceFocus,

    /// seat->focus_stack equivalent for tiling restore targets (MRU at index 0).
    ///
    /// Deliberately a *lazy* cache: entries are not pruned when the tree changes under
    /// them, only when a lookup finds they no longer resolve. Holding stale references is
    /// therefore expected, and no invariant may assert otherwise — the guarantee is that a
    /// lookup never *returns* one, which [`Self::inactive_tiling_restore_target`] enforces
    /// by skipping and dropping them as it scans.
    inactive_tiling_focus_stack: Vec<InactiveTilingReference>,

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
        Tiling = TilingSpaceRenderElement<R>,
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

/// Where command focus sits, independent of which layer is active.
///
/// The active *layer* (tiling vs floating) is always given by [`FloatingActive`]; this only
/// tracks whether focus rests on a concrete window/container or has been elevated to the
/// workspace itself. Keeping the elevation as a single bit (instead of the old pair of
/// `floating_workspace_context`/`tiling_workspace_context` booleans) makes the illegal
/// combinations — both sides elevated at once, or an elevation that disagrees with the active
/// layer — unrepresentable rather than merely asserted against.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum WorkspaceFocus {
    /// Focus is on a window or container within the active layer.
    OnContent,
    /// Focus is elevated to the workspace level (no concrete window/container selected).
    OnWorkspace,
}

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

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum CommandTarget {
    TilingWindow,
    TilingContainer,
    FloatingWindow,
    FloatingContainer,
    Workspace,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum CommandFamily {
    Focus,
    /// Split and layout commands route identically.
    Layout,
    MoveDirectional,
    MoveContainer,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum RouteDomain {
    Tiling,
    Floating,
    Workspace,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct ResolvedCommandRoute {
    command_target: CommandTarget,
    default_domain: RouteDomain,
    floating_workspace_container_selected: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum InactiveTilingRestoreSource {
    Stack,
    Current,
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

impl CommandTarget {
    fn domain(self) -> RouteDomain {
        match self {
            CommandTarget::TilingWindow | CommandTarget::TilingContainer => RouteDomain::Tiling,
            CommandTarget::FloatingWindow | CommandTarget::FloatingContainer => {
                RouteDomain::Floating
            }
            CommandTarget::Workspace => RouteDomain::Workspace,
        }
    }

    fn targets_container(self) -> bool {
        matches!(
            self,
            CommandTarget::TilingContainer | CommandTarget::FloatingContainer
        )
    }

    fn has_window_target(self) -> bool {
        matches!(
            self,
            CommandTarget::TilingWindow | CommandTarget::FloatingWindow
        )
    }
}

impl ResolvedCommandRoute {
    fn new(command_target: CommandTarget, floating_workspace_container_selected: bool) -> Self {
        Self {
            command_target,
            default_domain: command_target.domain(),
            floating_workspace_container_selected: matches!(
                command_target,
                CommandTarget::Workspace
            ) && floating_workspace_container_selected,
        }
    }

    fn domain_for_family(self, family: CommandFamily) -> RouteDomain {
        if self.floating_workspace_container_selected {
            return match family {
                CommandFamily::Focus => RouteDomain::Workspace,
                CommandFamily::Layout => RouteDomain::Floating,
                CommandFamily::MoveDirectional | CommandFamily::MoveContainer => {
                    self.default_domain
                }
            };
        }

        self.default_domain
    }

    fn preserves_floating_workspace_context_for_family(self, family: CommandFamily) -> bool {
        self.floating_workspace_container_selected && family == CommandFamily::Layout
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
        let working_area = compute_working_area(&output);

        let tiling = TilingSpace::new(
            view_size,
            working_area,
            scale.fractional_scale(),
            clock.clone(),
            options.clone(),
        );

        let floating = FloatingSpace::new(
            view_size,
            working_area,
            scale.fractional_scale(),
            clock.clone(),
            options.clone(),
        );

        let shadow_config =
            compute_workspace_shadow_config(options.overview.workspace_shadow, view_size);

        Self {
            tiling,
            floating,
            floating_is_active: FloatingActive::No,
            workspace_focus: WorkspaceFocus::OnContent,
            inactive_tiling_focus_stack: Vec::new(),
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

        let tiling = TilingSpace::new(
            view_size,
            working_area,
            scale.fractional_scale(),
            clock.clone(),
            options.clone(),
        );

        let floating = FloatingSpace::new(
            view_size,
            working_area,
            scale.fractional_scale(),
            clock.clone(),
            options.clone(),
        );

        let shadow_config =
            compute_workspace_shadow_config(options.overview.workspace_shadow, view_size);

        Self {
            tiling,
            floating,
            floating_is_active: FloatingActive::No,
            workspace_focus: WorkspaceFocus::OnContent,
            inactive_tiling_focus_stack: Vec::new(),
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

    fn assign_default_floating_size_if_missing(
        &self,
        tile: &mut Tile<W>,
        animate: bool,
    ) -> Option<Size<i32, Logical>> {
        if tile.floating_window_size.is_some() {
            return None;
        }

        // sway's `floating_natural_resize`: the window gets the size it asked for when it
        // mapped, not a fraction of anything. `container_floating_resize_and_center` then
        // centres it, which is what the caller already does. The working area is only the
        // ceiling — sway's `floating_maximum_size` defaults to the workspace.
        // `floating_calculate_constraints`, on its automatic settings: a floor of 75 by 50
        // whatever the client asked for, and the output as the ceiling. The floor is applied
        // last, as sway applies it, so it wins on an output too small to hold it.
        let working_size = self.floating.working_area().size;
        let mut size = tile.window().natural_size();
        size.w = size.w.min(working_size.w.floor() as i32).max(75);
        size.h = size.h.min(working_size.h.floor() as i32).max(50);

        // Respect min/max size constraints from the window.
        let min_size = tile.window().min_size();
        let max_size = tile.window().max_size();
        size.w = ensure_min_max_size(size.w, min_size.w, max_size.w);
        size.h = ensure_min_max_size(size.h, min_size.h, max_size.h);

        tile.floating_window_size = Some(size);
        tile.window_mut().request_size_once(size, animate);
        Some(size)
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
        self.tiling.advance_animations();
        self.floating.advance_animations();
    }

    pub fn are_animations_ongoing(&self) -> bool {
        self.tiling.are_animations_ongoing() || self.floating.are_animations_ongoing()
    }

    pub fn are_transitions_ongoing(&self) -> bool {
        self.tiling.are_transitions_ongoing() || self.floating.are_transitions_ongoing()
    }

    pub fn update_render_elements(&mut self, is_active: bool) {
        self.tiling
            .update_render_elements(is_active && !self.floating_is_active.get());

        let view_rect = Rectangle::from_size(self.view_size);
        self.floating
            .update_render_elements(is_active && self.floating_is_active.get(), view_rect);

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

        self.tiling.update_config(
            self.view_size,
            self.working_area,
            self.scale.fractional_scale(),
            options.clone(),
        );

        self.floating.update_config(
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
        self.tiling.update_shaders();
        self.floating.update_shaders();
        self.shadow.update_shaders();
    }

    pub fn windows(&self) -> impl Iterator<Item = &W> + '_ {
        self.tiles().map(Tile::window)
    }

    pub fn windows_mut(&mut self) -> impl Iterator<Item = &mut W> + '_ {
        self.tiles_mut().map(Tile::window_mut)
    }

    pub fn tiles(&self) -> impl Iterator<Item = &Tile<W>> + '_ {
        let tiling = self.tiling.tiles();
        let floating = self.floating.tiles();
        tiling.chain(floating)
    }

    pub fn tiles_mut(&mut self) -> impl Iterator<Item = &mut Tile<W>> + '_ {
        let tiling = self.tiling.tiles_mut();
        let floating = self.floating.tiles_mut();
        tiling.chain(floating)
    }

    pub fn is_floating(&self, id: &W::Id) -> bool {
        self.floating.has_window(id)
    }

    fn is_floating_target(&self, window: Option<&W::Id>) -> bool {
        window.map_or(self.floating_is_active.get(), |id| {
            self.floating.has_window(id)
        })
    }

    fn command_target(&self) -> CommandTarget {
        // Command routing: no floating command context exists when there are
        // no floating containers in the workspace.
        if self.floating.is_empty() || !self.floating_is_active.get() {
            if self.tiling_targets_workspace() {
                return CommandTarget::Workspace;
            }
            return if self.tiling.selected_is_container() {
                CommandTarget::TilingContainer
            } else {
                CommandTarget::TilingWindow
            };
        }

        if self.focus_is_elevated() {
            return CommandTarget::Workspace;
        }

        if self.floating.active_wrapper_selected() || self.floating.selected_is_container(None) {
            CommandTarget::FloatingContainer
        } else {
            CommandTarget::FloatingWindow
        }
    }

    fn resolved_command_route(&self) -> ResolvedCommandRoute {
        ResolvedCommandRoute::new(
            self.command_target(),
            self.floating.active_command_container_selected(),
        )
    }

    fn route_domain_for_family(&self, family: CommandFamily) -> RouteDomain {
        self.resolved_command_route().domain_for_family(family)
    }

    fn preserves_floating_workspace_context_for_family(&self, family: CommandFamily) -> bool {
        self.resolved_command_route()
            .preserves_floating_workspace_context_for_family(family)
    }

    /// Route a focus command to the active layer.
    ///
    /// Centralizes the tiling-side `sync_tiling_focus_context_from_tiling()` follow-up so it
    /// can never be forgotten by an individual focus method — the historical source of stale
    /// workspace-context bugs.
    fn dispatch_focus(
        &mut self,
        floating: impl FnOnce(&mut FloatingSpace<W>) -> bool,
        tiling: impl FnOnce(&mut TilingSpace<W>) -> bool,
    ) -> bool {
        match self.route_domain_for_family(CommandFamily::Focus) {
            RouteDomain::Workspace => false,
            RouteDomain::Floating => floating(&mut self.floating),
            RouteDomain::Tiling => {
                let moved = tiling(&mut self.tiling);
                self.sync_tiling_focus_context_from_tiling();
                moved
            }
        }
    }

    /// Route a directional move to the active layer. The floating layer always reports the move
    /// as handled; the tiling layer reports whether it actually moved.
    fn dispatch_move_directional(
        &mut self,
        floating: impl FnOnce(&mut FloatingSpace<W>),
        tiling: impl FnOnce(&mut TilingSpace<W>) -> bool,
    ) -> bool {
        match self.route_domain_for_family(CommandFamily::MoveDirectional) {
            RouteDomain::Workspace => false,
            RouteDomain::Floating => {
                floating(&mut self.floating);
                true
            }
            RouteDomain::Tiling => tiling(&mut self.tiling),
        }
    }

    /// Route by the active layer only, ignoring workspace focus elevation: these commands
    /// resolve their own target inside the layer (i3 falls back to the focused leaf).
    fn dispatch_active_layer<R>(
        &mut self,
        floating: impl FnOnce(&mut FloatingSpace<W>) -> R,
        tiling: impl FnOnce(&mut TilingSpace<W>) -> R,
    ) -> R {
        if self.floating_is_active.get() {
            floating(&mut self.floating)
        } else {
            tiling(&mut self.tiling)
        }
    }

    /// Route by the layer `window` lives in, defaulting to the active layer for `None`.
    fn dispatch_for_window<R>(
        &mut self,
        window: Option<&W::Id>,
        floating: impl FnOnce(&mut FloatingSpace<W>) -> R,
        tiling: impl FnOnce(&mut TilingSpace<W>) -> R,
    ) -> R {
        if self.is_floating_target(window) {
            floating(&mut self.floating)
        } else {
            tiling(&mut self.tiling)
        }
    }

    /// Route a container move. Only the tiling layer reorders containers; the floating layer and
    /// the workspace itself ignore it.
    fn dispatch_move_container<R: Default>(
        &mut self,
        tiling: impl FnOnce(&mut TilingSpace<W>) -> R,
    ) -> R {
        match self.route_domain_for_family(CommandFamily::MoveContainer) {
            RouteDomain::Tiling => tiling(&mut self.tiling),
            RouteDomain::Workspace | RouteDomain::Floating => R::default(),
        }
    }

    pub fn focus_mode_toggle_targets_floating(&self) -> bool {
        match self.resolved_command_route().command_target {
            CommandTarget::Workspace => true,
            CommandTarget::FloatingWindow | CommandTarget::FloatingContainer => false,
            CommandTarget::TilingWindow | CommandTarget::TilingContainer => true,
        }
    }

    pub fn current_output(&self) -> Option<&Output> {
        self.output.as_ref()
    }

    pub fn active_window(&self) -> Option<&W> {
        if self.floating_is_active.get() {
            self.floating.active_window()
        } else {
            self.tiling.active_window()
        }
    }

    pub fn active_window_mut(&mut self) -> Option<&mut W> {
        if self.floating_is_active.get() {
            self.floating.active_window_mut()
        } else {
            self.tiling.active_window_mut()
        }
    }

    pub fn active_selection_is_container(&self) -> bool {
        self.resolved_command_route()
            .command_target
            .targets_container()
    }

    pub fn active_command_has_window_target(&self) -> bool {
        self.resolved_command_route()
            .command_target
            .has_window_target()
    }

    pub fn close_window_ids_for_active_selection(&self) -> Vec<W::Id> {
        match self.resolved_command_route().command_target {
            CommandTarget::Workspace => {
                return self.windows().map(|window| window.id().clone()).collect();
            }
            CommandTarget::FloatingWindow | CommandTarget::FloatingContainer => {
                let ids = self.floating.close_window_ids_for_active_selection();
                if !ids.is_empty() {
                    return ids;
                }
            }
            CommandTarget::TilingWindow | CommandTarget::TilingContainer => {
                let ids = self.tiling.close_window_ids_for_active_selection();
                if !ids.is_empty() {
                    return ids;
                }
            }
        }

        self.active_window()
            .map(|window| vec![window.id().clone()])
            .unwrap_or_default()
    }

    pub fn is_active_pending_fullscreen(&self) -> bool {
        self.tiling.is_active_pending_fullscreen()
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
            self.tiling.update_config(
                size,
                working_area,
                scale.fractional_scale(),
                self.options.clone(),
            );
            self.floating.update_config(
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
        let command_target = self.command_target();
        let workspace_command_context = matches!(command_target, CommandTarget::Workspace);

        match target {
            WorkspaceAddWindowTarget::Auto => {
                // Model rule: only a focused floating window inside an explicitly
                // split/grouped floating container auto-groups the next normal
                // window into floating. Floating container/workspace contexts do not.
                let grouped_floating = !is_floating
                    && floating_active
                    && !self.floating.active_container_is_workspace_floated()
                    && self.floating.active_container_allows_splits()
                    && (matches!(command_target, CommandTarget::FloatingWindow)
                        || self.floating.active_wrapper_selected());
                let wants_floating = is_floating || grouped_floating;
                let has_tiling_fullscreen = self.tiling.has_fullscreen_window();
                if !wants_floating {
                    tile.set_scratchpad(false);
                }
                tile.restore_to_floating = wants_floating;

                let keep_floating_focus = floating_active
                    && !wants_floating
                    && (workspace_command_context
                        || matches!(command_target, CommandTarget::FloatingContainer));
                // Model rule: when a floating container is selected (focus-parent context),
                // opening a new floating window inserts into that container without stealing
                // selection/focus from the container command target.
                let keep_floating_container_selection =
                    floating_active && wants_floating && self.floating.selected_is_container(None);
                let activate = if keep_floating_focus || keep_floating_container_selection {
                    false
                } else if !wants_floating && has_tiling_fullscreen {
                    // Model rule: while a tiling window is fullscreen, newly opened tiling windows
                    // should not steal focus.
                    false
                } else {
                    // Don't steal focus from an active fullscreen window.
                    activate.map_smart(|| !self.is_active_pending_fullscreen())
                };

                // If the tile is pending maximized or fullscreen, open it in the tiling layout
                // where it can do that.
                if wants_floating
                    && tile.window().pending_sizing_mode().is_normal()
                    && !tile.pending_maximized
                {
                    if grouped_floating {
                        self.floating.add_tile_to_active_container(tile, activate);
                    } else {
                        self.floating.add_tile(tile, activate);
                    }

                    if activate || self.tiling.is_empty() {
                        self.floating_is_active = FloatingActive::Yes;
                        self.workspace_focus = WorkspaceFocus::OnContent;
                    }
                } else {
                    let tiling_was_empty = self.tiling.is_empty();
                    self.tiling
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
                if !is_floating {
                    tile.set_scratchpad(false);
                }
                tile.restore_to_floating = is_floating;
                let activate = activate.map_smart(|| false);
                self.tiling
                    .add_tile(Some(col_idx), tile, activate, width, is_full_width, None);

                if activate {
                    self.activate_tiling_for_new_content();
                }
            }
            WorkspaceAddWindowTarget::NextTo(next_to) => {
                let floating_has_window = self.floating.has_window(next_to);
                let grouped_floating_target =
                    floating_has_window && self.floating.container_allows_splits(next_to);
                let wants_floating = is_floating || grouped_floating_target;
                if !wants_floating {
                    tile.set_scratchpad(false);
                }
                tile.restore_to_floating = wants_floating;

                let activate = activate
                    .map_smart(|| self.active_window().is_some_and(|win| win.id() == next_to));

                if wants_floating
                    && tile.window().pending_sizing_mode().is_normal()
                    && !tile.pending_maximized
                {
                    if floating_has_window {
                        if grouped_floating_target {
                            self.floating
                                .add_tile_to_container_of(next_to, tile, activate);
                        } else {
                            self.floating.add_tile_above(next_to, tile, activate);
                        }
                    } else {
                        if let Some((next_to_tile, render_pos, _visible)) = self
                            .tiling
                            .tiles_with_render_positions()
                            .find(|(tile, _, _)| tile.window().id() == next_to)
                        {
                            // Position the new tile in the center above the next_to tile. Think
                            // a dialog opening on top of a window.
                            //
                            // FIXME: use static pos
                            let tile_size = tile.tile_size();
                            let pos = render_pos
                                + (next_to_tile.tile_size().to_point() - tile_size.to_point())
                                    .downscale(2.);
                            let pos = self.floating.clamp_within_working_area(pos, tile_size);
                            let pos = self.floating.logical_to_size_frac(pos);
                            tile.floating_pos = Some(pos);
                        } else {
                            error!(
                                "next_to target disappeared while placing a new floating window"
                            );
                        }
                        self.floating.add_tile(tile, activate);
                    }

                    if activate || self.tiling.is_empty() {
                        self.floating_is_active = FloatingActive::Yes;
                        self.workspace_focus = WorkspaceFocus::OnContent;
                    }
                } else if floating_has_window {
                    self.tiling
                        .add_tile(None, tile, activate, width, is_full_width, None);

                    if activate {
                        self.activate_tiling_for_new_content();
                    }
                } else {
                    if self
                        .tiling
                        .tiles()
                        .any(|tile| tile.window().id() == next_to)
                    {
                        self.tiling.add_tile_right_of(
                            next_to,
                            tile,
                            activate,
                            width,
                            is_full_width,
                        );
                    } else {
                        error!("next_to target disappeared while placing a new tiled window");
                        self.tiling
                            .add_tile(None, tile, activate, width, is_full_width, None);
                    }

                    if activate {
                        self.floating_is_active = FloatingActive::No;
                        self.sync_tiling_focus_context_from_tiling();
                    }
                }
            }
        }
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
        self.tiling
            .add_tile_to_root_container(root_idx, tile_idx, tile, activate);

        if activate {
            self.floating_is_active = FloatingActive::No;
            self.sync_tiling_focus_context_from_tiling();
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
        self.tiling.insert_parent_info_for_window(window)
    }

    fn remember_inactive_tiling_reference(&mut self, reference: InactiveTilingReference) {
        let key = reference.node_key();
        self.inactive_tiling_focus_stack
            .retain(|existing| existing.node_key() != key);
        self.inactive_tiling_focus_stack.insert(0, reference);
        if self.inactive_tiling_focus_stack.len() > 64 {
            self.inactive_tiling_focus_stack.truncate(64);
        }
    }

    fn inactive_tiling_restore_target(
        &mut self,
    ) -> Option<(InsertParentInfo, InactiveTilingRestoreSource)> {
        let debug_restore = std::env::var_os("TIRI_PARITY_DEBUG_RESTORE").is_some();

        // If the workspace has no tiling nodes, there is no inactive tiling target.
        if self.tiling.windows().next().is_none() {
            if debug_restore {
                eprintln!("restore_target: no tiling windows");
            }
            return None;
        }

        if debug_restore {
            eprintln!(
                "restore_target: stack={:?}",
                self.inactive_tiling_focus_stack,
            );
        }

        // Model rule: restore target for floating->tiling comes from the seat
        // inactive focus stack first (seat_get_focus_inactive_tiling()).
        let idx = 0;
        while idx < self.inactive_tiling_focus_stack.len() {
            let reference = &self.inactive_tiling_focus_stack[idx];
            if let Some(info) = self
                .tiling
                .insert_parent_info_from_inactive_tiling_reference_strict(reference)
            {
                if self
                    .tiling
                    .inactive_tiling_reference_is_root_container_strict(reference)
                {
                    if let Some((candidate, candidate_info)) = self
                        .inactive_tiling_focus_stack
                        .iter()
                        .skip(idx + 1)
                        .filter_map(|candidate| {
                            let info = self
                                .tiling
                                .insert_parent_info_from_inactive_tiling_reference(candidate)?;
                            (!info.parent_path.is_empty()).then_some((candidate, info))
                        })
                        .max_by_key(|(_, info)| info.parent_path.len())
                    {
                        if debug_restore {
                            eprintln!(
                                "restore_target: prefer_specific_over_root root={reference:?} specific={candidate:?} info={candidate_info:?}"
                            );
                        }
                        return Some((candidate_info, InactiveTilingRestoreSource::Stack));
                    }
                }
                if debug_restore {
                    eprintln!("restore_target: from_stack={reference:?} info={info:?}");
                }
                return Some((info, InactiveTilingRestoreSource::Stack));
            }
            if debug_restore {
                eprintln!("restore_target: drop_stale={reference:?}");
            }
            self.inactive_tiling_focus_stack.remove(idx);
        }

        // Fallback only when the inactive stack has no valid tiling references.
        if let Some(reference) = self
            .tiling
            .inactive_tiling_reference_for_selected_or_focused()
        {
            let info = self
                .tiling
                .insert_parent_info_from_inactive_tiling_reference(&reference);
            if debug_restore {
                eprintln!("restore_target: from_current={reference:?} info={info:?}");
            }
            return info.map(|info| (info, InactiveTilingRestoreSource::Current));
        }

        if debug_restore {
            eprintln!("restore_target: none");
        }
        None
    }

    fn remember_current_tiling_reference(&mut self) {
        if matches!(
            self.resolved_command_route().command_target,
            CommandTarget::Workspace
        ) {
            return;
        }

        let chain = self
            .tiling
            .inactive_tiling_reference_chain_for_focused_reference();
        for reference in chain.into_iter().rev() {
            self.remember_inactive_tiling_reference(reference);
        }
    }

    fn remember_current_tiling_focused_leaf_reference(&mut self) {
        let chain = self
            .tiling
            .inactive_tiling_reference_chain_for_focused_leaf();
        for reference in chain.into_iter().rev() {
            self.remember_inactive_tiling_reference(reference);
        }
    }

    fn sync_tiling_focus_context_from_tiling(&mut self) {
        self.workspace_focus = WorkspaceFocus::OnContent;
        self.remember_current_tiling_reference();
    }

    pub(super) fn seat_focus_tiling_chain(&self) -> Vec<super::container::InactiveTilingReference> {
        self.tiling
            .inactive_tiling_reference_chain_for_focused_reference()
    }

    pub(super) fn has_tiling_reference(
        &self,
        reference: &super::container::InactiveTilingReference,
        strict: bool,
    ) -> bool {
        self.tiling.has_inactive_tiling_reference(reference, strict)
    }

    pub(super) fn focus_tiling_reference(
        &mut self,
        reference: &super::container::InactiveTilingReference,
        strict: bool,
    ) -> bool {
        let focused = self
            .tiling
            .focus_inactive_tiling_reference(reference, strict);
        if focused {
            self.floating_is_active = FloatingActive::No;
            self.sync_tiling_focus_context_from_tiling();
        }
        focused
    }

    fn window_has_fullscreen_focus_scope(&self, window: &W) -> bool {
        self.tiling.is_fullscreen(window)
            || window.pending_sizing_mode().is_fullscreen()
            || window.is_pending_windowed_fullscreen()
    }

    pub(super) fn tiling_reference_focusable(
        &self,
        reference: &super::container::InactiveTilingReference,
        strict: bool,
    ) -> bool {
        let any_fullscreen = self
            .windows()
            .any(|window| self.window_has_fullscreen_focus_scope(window));
        if !any_fullscreen {
            return true;
        }

        self.tiling
            .window_for_inactive_tiling_reference(reference, strict)
            .is_some_and(|window| self.window_has_fullscreen_focus_scope(window))
    }

    pub(super) fn focus_floating_window(&mut self, id: &W::Id, raise: bool) -> bool {
        let focused = if raise {
            self.floating.activate_window(id)
        } else {
            self.floating.activate_window_without_raising(id)
        };
        if focused {
            self.floating_is_active = FloatingActive::Yes;
            self.workspace_focus = WorkspaceFocus::OnContent;
        }
        focused
    }

    pub(super) fn tiling_reference_targets_window(
        &self,
        reference: &super::container::InactiveTilingReference,
        strict: bool,
        id: &W::Id,
    ) -> bool {
        self.tiling
            .window_for_inactive_tiling_reference(reference, strict)
            .is_some_and(|window| window.id() == id)
    }

    /// Swap a dragged tile with the leaf at `path`, sending the displaced tile back to
    /// `origin` (where the dragged tile came from).
    ///
    /// Hands the tile back as `Err` when `path` no longer addresses a leaf, leaving the
    /// tree untouched so the caller can fall back to a plain insert.
    // The Err variant carries the tile back to the caller; boxing it would only add an
    // allocation to the failure path.
    #[allow(clippy::result_large_err)]
    pub(super) fn tiling_swap_tile_at_path(
        &mut self,
        path: &[usize],
        tile: Tile<W>,
        origin: &InsertParentInfo,
    ) -> Result<(), Tile<W>> {
        if !self.tiling.is_leaf_at_path(path) {
            return Err(tile);
        }
        let Some(displaced) = self.tiling.replace_tile_at_path(path, tile) else {
            // is_leaf_at_path just said otherwise; the tile is already gone into the tree.
            return Ok(());
        };
        self.tiling
            .insert_tile_with_parent_info(origin, displaced, false);
        Ok(())
    }

    pub fn add_tile_split(
        &mut self,
        target_path: &[usize],
        direction: Direction,
        mut tile: Tile<W>,
        activate: bool,
    ) -> bool {
        tile.set_scratchpad(false);
        self.enter_output_for_window(tile.window());
        tile.restore_to_floating = false;

        let inserted = self
            .tiling
            .insert_tile_split(target_path, direction, tile, activate);

        if inserted && activate {
            self.floating_is_active = FloatingActive::No;
            self.sync_tiling_focus_context_from_tiling();
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

        let inserted = self
            .tiling
            .insert_tile_split_root(direction, tile, activate);

        if inserted && activate {
            self.floating_is_active = FloatingActive::No;
            self.sync_tiling_focus_context_from_tiling();
        }

        inserted
    }

    pub fn add_root_tiling_subtree(&mut self, subtree: RootTilingSubtree<W>, activate: bool) {
        for tile in subtree.tiles() {
            self.enter_output_for_window(tile.window());
        }

        self.tiling
            .add_root_tiling_subtree(None, subtree, activate, None);

        if activate {
            self.floating_is_active = FloatingActive::No;
            self.sync_tiling_focus_context_from_tiling();
        }
    }

    pub fn add_column(&mut self, column: Column<W>, activate: bool) {
        self.add_root_tiling_subtree(column.into(), activate);
    }

    fn update_focus_floating_tiling_after_removing(&mut self, removed_from_floating: bool) {
        // An elevation can only belong to the active layer (the inactive layer's elevation is
        // already dropped by construction), so clear it when that active layer empties out.
        if self.tiling.is_empty() {
            self.tiling.clear_selection_context();
            if !self.floating_is_active.get() {
                self.workspace_focus = WorkspaceFocus::OnContent;
            }
        }
        if self.floating.is_empty() {
            self.floating.clear_selection_context();
            if self.floating_is_active.get() {
                self.workspace_focus = WorkspaceFocus::OnContent;
            }
        }

        if removed_from_floating {
            if self.floating.is_empty() {
                self.floating_is_active = FloatingActive::No;
                self.sync_tiling_focus_context_from_tiling();
            }
        } else {
            // Tiling should remain focused if both are empty.
            if self.tiling.is_empty() && !self.floating.is_empty() {
                self.floating_is_active = FloatingActive::Yes;
            }
        }
    }

    pub fn remove_tile(&mut self, id: &W::Id, transaction: Transaction) -> RemovedTile<W> {
        let mut from_floating = false;
        let removed = if self.floating.has_window(id) {
            from_floating = true;
            self.floating.remove_tile(id)
        } else {
            self.tiling.remove_tile(id, transaction)
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
            self.floating.remove_active_tile()?
        } else {
            self.tiling.remove_active_tile(transaction)?
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

        let subtree = self.tiling.remove_active_root_tiling_subtree()?;

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
            self.floating.new_window_size(width, height, rules)
        } else {
            self.tiling.new_window_size(width, height, rules)
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
            } else {
                let size =
                    self.new_window_size(width, height, is_floating, rules, (min_size, max_size));
                state.size = Some(size);
            }

            if is_floating {
                state.bounds = Some(self.floating.new_window_toplevel_bounds(rules));
            } else {
                state.bounds = Some(self.tiling.new_window_toplevel_bounds(rules));
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
        self.dispatch_focus(|f| f.focus_left(), |t| t.focus_left())
    }

    pub fn focus_left_no_wrap(&mut self) -> bool {
        self.dispatch_focus(|f| f.focus_left_no_wrap(), |t| t.focus_left_no_wrap())
    }

    pub fn focus_right(&mut self) -> bool {
        self.dispatch_focus(|f| f.focus_right(), |t| t.focus_right())
    }

    pub fn focus_right_no_wrap(&mut self) -> bool {
        self.dispatch_focus(|f| f.focus_right_no_wrap(), |t| t.focus_right_no_wrap())
    }

    pub fn focus_root_container_first(&mut self) {
        self.dispatch_focus(
            |f| {
                f.focus_leftmost();
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
            |f| {
                f.focus_rightmost();
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
        self.tiling.focus_root_container(index);
        self.sync_tiling_focus_context_from_tiling();
    }

    pub fn focus_leaf_in_root_container(&mut self, index: u8) {
        if self.floating_is_active.get() {
            return;
        }
        self.tiling.focus_leaf_in_root_container(index);
        self.sync_tiling_focus_context_from_tiling();
    }

    pub fn focus_column(&mut self, index: usize) {
        self.focus_root_container(index);
    }

    pub fn focus_window_in_column(&mut self, index: u8) {
        self.focus_leaf_in_root_container(index);
    }

    pub fn focus_down(&mut self) -> bool {
        self.dispatch_focus(|f| f.focus_down(), |t| t.focus_down())
    }

    pub fn focus_up(&mut self) -> bool {
        self.dispatch_focus(|f| f.focus_up(), |t| t.focus_up())
    }

    pub fn focus_down_or_left(&mut self) {
        self.dispatch_focus(
            |f| {
                f.focus_down();
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
            |f| {
                f.focus_down();
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
            |f| {
                f.focus_up();
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
            |f| {
                f.focus_up();
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
            |f| {
                f.focus_topmost();
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
            |f| {
                f.focus_bottommost();
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
        self.dispatch_focus(|f| f.focus_up_no_wrap(), |t| t.focus_up_no_wrap())
    }

    pub fn focus_down_no_wrap(&mut self) -> bool {
        self.dispatch_focus(|f| f.focus_down_no_wrap(), |t| t.focus_down_no_wrap())
    }

    pub(super) fn focus_entry_from_output_direction(&mut self, direction: Direction) -> bool {
        if self.tiling.has_fullscreen_window() {
            // Fullscreen workspace targets resolve to the inactive focus under
            // the fullscreen subtree. Keep tiling active as-is.
            self.floating_is_active = FloatingActive::No;
            self.sync_tiling_focus_context_from_tiling();
            return true;
        }

        let Some((root_layout, child_count)) = self.tiling.root_layout_and_child_count() else {
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
            // inactive tiling.
            return false;
        }

        match direction {
            Direction::Left | Direction::Up => self.tiling.focus_root_container_last(),
            Direction::Right | Direction::Down => self.tiling.focus_root_container_first(),
        }
        self.floating_is_active = FloatingActive::No;
        self.sync_tiling_focus_context_from_tiling();
        true
    }

    pub(super) fn has_tiling_windows(&self) -> bool {
        !self.tiling.is_empty()
    }

    pub(super) fn focus_workspace_node(&mut self) {
        self.tiling.clear_selection_context();
        self.floating.clear_selection_context();
        if self.floating.is_empty() {
            self.floating_is_active = FloatingActive::No;
            self.workspace_focus = WorkspaceFocus::OnContent;
            return;
        }

        // The workspace becomes command context while floating mode stays active.
        self.floating_is_active = FloatingActive::Yes;
        self.workspace_focus = WorkspaceFocus::OnWorkspace;
    }

    fn focus_is_elevated(&self) -> bool {
        self.workspace_focus == WorkspaceFocus::OnWorkspace
    }

    /// Switch the active layer to tiling, with the new window as what commands are aimed at.
    ///
    /// Measured against sway 1.11: `focus parent` selects the workspace, but opening a
    /// window ends that — the next command goes to the window. The elevation records that
    /// the user asked for the workspace at some point; a window taking focus answers the
    /// question.
    fn activate_tiling_for_new_content(&mut self) {
        self.floating_is_active = FloatingActive::No;
        self.workspace_focus = WorkspaceFocus::OnContent;
    }

    pub(super) fn is_floating_workspace_context_active(&self) -> bool {
        self.floating_is_active.get() && self.focus_is_elevated()
    }

    /// Whether tiling commands are aimed at the workspace itself.
    ///
    /// Read from the tree's selection wherever the tree can express it, so the routing can
    /// never disagree with what the command will actually do. The stored elevation is only
    /// consulted for the one state the tree has no node for — a workspace whose single child
    /// is a window — and it stops applying as soon as the tree gains a root container,
    /// which is what opening a second window does.
    pub(super) fn tiling_targets_workspace(&self) -> bool {
        self.tiling.workspace_is_selected()
            || (self.tiling.focus_is_root_leaf() && self.focus_is_elevated())
    }

    pub(super) fn is_tiling_workspace_context_active(&self) -> bool {
        !self.floating_is_active.get() && self.tiling_targets_workspace()
    }

    pub fn focus_window_by_id(&mut self, id: &W::Id) -> bool {
        if self.floating.has_window(id) && self.floating.focus_window_by_id(id) {
            self.floating_is_active = FloatingActive::Yes;
            self.workspace_focus = WorkspaceFocus::OnContent;
            return true;
        }

        if self.tiling.activate_window(id) {
            self.floating_is_active = FloatingActive::No;
            self.sync_tiling_focus_context_from_tiling();
            return true;
        }

        false
    }

    pub fn move_left(&mut self) -> bool {
        self.dispatch_move_directional(|f| f.move_left(), |t| t.move_left())
    }

    pub fn move_right(&mut self) -> bool {
        self.dispatch_move_directional(|f| f.move_right(), |t| t.move_right())
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
        self.dispatch_move_directional(|f| f.move_down(), |t| t.move_down())
    }

    pub fn move_up(&mut self) -> bool {
        self.dispatch_move_directional(|f| f.move_up(), |t| t.move_up())
    }

    pub fn consume_or_expel_window_left(&mut self, window: Option<&W::Id>) {
        self.dispatch_for_window(
            window,
            |f| f.consume_or_expel_window_left(window),
            |t| t.consume_or_expel_window_left(window),
        );
    }

    pub fn consume_or_expel_window_right(&mut self, window: Option<&W::Id>) {
        self.dispatch_for_window(
            window,
            |f| f.consume_or_expel_window_right(window),
            |t| t.consume_or_expel_window_right(window),
        );
    }

    pub fn consume_into_container(&mut self) {
        self.dispatch_active_layer(|f| f.consume_into_column(), |t| t.consume_into_column());
    }

    pub fn consume_into_column(&mut self) {
        self.consume_into_container();
    }

    pub fn expel_from_container(&mut self) {
        self.dispatch_active_layer(|f| f.expel_from_column(), |t| t.expel_from_column());
    }

    pub fn expel_from_column(&mut self) {
        self.expel_from_container();
    }

    pub fn swap_window_in_direction(&mut self, direction: Direction) {
        self.dispatch_move_directional(
            |f| f.swap_window_in_direction(direction),
            |t| {
                t.swap_window_in_direction(direction);
                true
            },
        );
    }

    pub fn toggle_column_tabbed_display(&mut self) {
        self.dispatch_active_layer(
            |f| f.toggle_column_tabbed_display(),
            |t| t.toggle_column_tabbed_display(),
        );
    }

    pub fn set_column_display(&mut self, display: ColumnDisplay) {
        self.dispatch_active_layer(
            |f| f.set_column_display(display),
            |t| t.set_column_display(display),
        );
    }

    pub fn center_column(&mut self) {
        self.dispatch_active_layer(|f| f.center_window(None), |t| t.center_column());
    }

    pub fn center_window(&mut self, id: Option<&W::Id>) {
        self.dispatch_for_window(id, |f| f.center_window(id), |t| t.center_window(id));
    }

    pub fn center_visible_columns(&mut self) {
        self.dispatch_active_layer(|_| {}, |t| t.center_visible_columns());
    }

    pub fn toggle_width(&mut self, forwards: bool) {
        self.dispatch_active_layer(
            |f| f.toggle_window_width(None, forwards),
            |t| t.toggle_width(forwards),
        );
    }

    pub fn toggle_full_width(&mut self) {
        // Floating is left unimplemented for now. For good UX, this probably needs moving the
        // tile to be against the left edge of the working area while it is full-width.
        self.dispatch_active_layer(|_| {}, |t| t.toggle_full_width());
    }

    pub fn set_column_width(&mut self, change: SizeChange) {
        self.dispatch_active_layer(
            |f| f.set_window_width(None, change, true),
            |t| t.set_column_width(change),
        );
    }

    pub fn set_window_width(&mut self, window: Option<&W::Id>, change: SizeChange) {
        self.dispatch_for_window(
            window,
            |f| f.set_window_width(window, change, true),
            |t| t.set_window_width(window, change),
        );
    }

    pub fn set_window_height(&mut self, window: Option<&W::Id>, change: SizeChange) {
        self.dispatch_for_window(
            window,
            |f| f.set_window_height(window, change, true),
            |t| t.set_window_height(window, change),
        );
    }

    pub fn reset_window_height(&mut self, window: Option<&W::Id>) {
        self.dispatch_for_window(window, |_| {}, |t| t.reset_window_height(window));
    }

    pub fn toggle_window_width(&mut self, window: Option<&W::Id>, forwards: bool) {
        self.dispatch_for_window(
            window,
            |f| f.toggle_window_width(window, forwards),
            |t| t.toggle_window_width(window, forwards),
        );
    }

    pub fn toggle_window_height(&mut self, window: Option<&W::Id>, forwards: bool) {
        self.dispatch_for_window(
            window,
            |f| f.toggle_window_height(window, forwards),
            |t| t.toggle_window_height(window, forwards),
        );
    }

    pub fn expand_column_to_available_width(&mut self) {
        self.dispatch_active_layer(|_| {}, |t| t.expand_column_to_available_width());
    }

    pub fn focus_parent(&mut self) {
        match self.command_target() {
            CommandTarget::FloatingWindow | CommandTarget::FloatingContainer => {
                // Model rule: when floating focus reaches above the floating container,
                // command context moves to workspace while floating mode remains active.
                self.workspace_focus = if self.floating.focus_parent() {
                    WorkspaceFocus::OnContent
                } else {
                    WorkspaceFocus::OnWorkspace
                };
            }
            CommandTarget::TilingWindow | CommandTarget::TilingContainer => {
                if self.tiling.focus_parent_targets_workspace() {
                    let _ = self.tiling.select_root_container();
                    self.workspace_focus = WorkspaceFocus::OnWorkspace;
                } else {
                    self.tiling.focus_parent();
                    self.sync_tiling_focus_context_from_tiling();
                }
            }
            CommandTarget::Workspace => {}
        }
    }

    pub fn focus_child(&mut self) {
        match self.command_target() {
            CommandTarget::FloatingWindow | CommandTarget::FloatingContainer => {
                self.floating.focus_child();
            }
            CommandTarget::TilingWindow | CommandTarget::TilingContainer => {
                self.tiling.focus_child();
                self.sync_tiling_focus_context_from_tiling();
            }
            CommandTarget::Workspace => {
                if self.floating_is_active.get() {
                    if self.focus_is_elevated() && self.floating.focus_child() {
                        self.workspace_focus = WorkspaceFocus::OnContent;
                    }
                    return;
                }
                // Reaching this arm without the floating side active already means the
                // workspace is what the tree has selected, so there is nothing else to ask.
                if !self.tiling.is_empty() {
                    let _ = self.tiling.focus_child();
                    self.sync_tiling_focus_context_from_tiling();
                }
            }
        }
    }

    /// Route a split/layout command: workspace-level targets apply to the workspace layout,
    /// floating targets drop focus elevation unless the selected floating workspace container
    /// preserves it. Model rule: split/layout commands work in both floating and tiling.
    fn dispatch_layout(
        &mut self,
        workspace: impl FnOnce(&mut TilingSpace<W>),
        tiling: impl FnOnce(&mut TilingSpace<W>),
        floating: impl FnOnce(&mut FloatingSpace<W>),
    ) {
        match self.route_domain_for_family(CommandFamily::Layout) {
            RouteDomain::Workspace => workspace(&mut self.tiling),
            RouteDomain::Tiling => tiling(&mut self.tiling),
            RouteDomain::Floating => {
                if !self.preserves_floating_workspace_context_for_family(CommandFamily::Layout) {
                    self.workspace_focus = WorkspaceFocus::OnContent;
                }
                floating(&mut self.floating);
            }
        }
    }

    pub fn split_horizontal(&mut self) {
        self.dispatch_layout(
            |t| t.split_workspace_horizontal(),
            |t| t.split_horizontal(),
            |f| f.split_horizontal(),
        );
    }

    pub fn split_vertical(&mut self) {
        self.dispatch_layout(
            |t| t.split_workspace_vertical(),
            |t| t.split_vertical(),
            |f| f.split_vertical(),
        );
    }

    /// `split toggle`, which sway does not implement as an operation of its own.
    ///
    /// `cmd_split` reads the layout of the parent of whatever the command is aimed at and
    /// runs `split h` when it is vertical and `split v` otherwise — including when there is
    /// no parent to read, which is the workspace itself. So this chooses, and everything
    /// after it is the ordinary split path, wrapping and all.
    pub fn split_toggle(&mut self) {
        let parent_is_vertical =
            self.tiling.command_target_parent_layout() == Some(Layout::SplitV);
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
            |f| f.set_layout_mode(layout),
        );
    }

    pub fn toggle_split_layout(&mut self) {
        self.dispatch_layout(
            |t| t.toggle_workspace_split_layout(),
            |t| t.toggle_split_layout(),
            |f| f.toggle_split_layout(),
        );
    }

    pub fn toggle_layout_all(&mut self) {
        self.dispatch_layout(
            |t| t.toggle_workspace_layout_all(),
            |t| t.toggle_layout_all(),
            |f| f.toggle_layout_all(),
        );
    }

    pub fn set_fullscreen(&mut self, window: &W::Id, is_fullscreen: bool) {
        if self.floating.has_window(window) {
            self.floating.set_fullscreen(window, is_fullscreen);
            return;
        }

        if !is_fullscreen {
            // The window is in the tiling layout and we're requesting an unfullscreen. If it is
            // indeed fullscreen (i.e. this isn't a duplicate unfullscreen request), then we may
            // need to unfullscreen into floating.
            let tile = self
                .tiling
                .tiles()
                .find(|tile| tile.window().id() == window)
                .unwrap();

            // When going from fullscreen to maximized, don't consider restore_to_floating yet.
            // pending_sizing_mode() is asynchronous, so also check tiling.is_fullscreen() to
            // handle requests while the client is catching up.
            let is_fullscreen_now = self.tiling.is_fullscreen(tile.window())
                || tile.window().pending_sizing_mode().is_fullscreen();
            if is_fullscreen_now && !tile.pending_maximized && tile.restore_to_floating {
                // Unfullscreen and float in one call so it has a chance to notice and request a
                // (0, 0) size, rather than the tiling tile size.
                self.toggle_window_floating(Some(window));
                return;
            }
        }

        self.tiling.set_fullscreen(window, is_fullscreen);
    }

    pub fn toggle_fullscreen(&mut self, window: &W::Id) {
        if self.floating.has_window(window) {
            let current = self.floating.is_fullscreen(window);
            self.set_fullscreen(window, !current);
            return;
        }

        let tile = self
            .tiles()
            .find(|tile| tile.window().id() == window)
            .unwrap();
        // Use tiling.is_fullscreen() as the source of truth instead of pending_sizing_mode()
        // because pending_sizing_mode() updates asynchronously after animations complete.
        let current = self.tiling.is_fullscreen(tile.window());
        self.set_fullscreen(window, !current);
    }

    pub fn set_windowed_fullscreen(&mut self, window: &W::Id, is_fullscreen: bool) {
        self.set_fullscreen(window, is_fullscreen);
    }

    pub fn set_maximized(&mut self, window: &W::Id, maximize: bool) {
        let mut restore_to_floating = false;
        if self.floating.has_window(window) {
            if maximize {
                restore_to_floating = true;
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
                .tiling
                .tiles()
                .find(|tile| tile.window().id() == window)
                .unwrap();
            if tile.window().pending_sizing_mode().is_fullscreen() {
                self.tiling.set_maximized(window, maximize);
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
            .tiling
            .tiles()
            .find(|tile| tile.window().id() == window)
            .unwrap();
        let was_normal = tile.window().pending_sizing_mode().is_normal();

        self.tiling.set_maximized(window, maximize);

        // When going from normal to maximized, remember if we should unmaximize to floating.
        let tile = self
            .tiling
            .tiles_mut()
            .find(|tile| tile.window().id() == window)
            .unwrap();
        if was_normal && tile.pending_maximized {
            tile.restore_to_floating = restore_to_floating;
        }
    }

    pub fn toggle_maximized(&mut self, window: &W::Id) {
        let current = self
            .tiling
            .tiles()
            .find(|tile| tile.window().id() == window)
            .is_some_and(|tile| tile.pending_maximized);

        self.set_maximized(window, !current);
    }

    pub fn toggle_window_floating(&mut self, id: Option<&W::Id>) {
        let mut command_context = self.resolved_command_route().default_domain;
        let preserve_workspace_context_on_unfloat = command_context == RouteDomain::Workspace;

        if id.is_none() && command_context == RouteDomain::Workspace {
            // Floating command routing:
            // - if a floating container is still selected at command level, target it;
            // - otherwise workspace context with tiling children targets workspace tiling;
            // - workspace context with empty tiling is a no-op.
            if self.floating.active_command_container_selected() {
                command_context = RouteDomain::Floating;
            } else if self.tiling.is_empty() {
                return;
            }
        }

        let explicit_window = id.is_some();
        let active_id = self.active_window().map(|win| win.id().clone());
        let target_is_active = id.is_none_or(|id| Some(id) == active_id.as_ref());
        let preserve_selection_path_on_unfloat =
            if !explicit_window && target_is_active && command_context == RouteDomain::Floating {
                self.floating
                    .active_command_container_path()
                    // Model rule: unfloating from a floating wrapper/root focus
                    // must not restore a workspace-level container selection.
                    .filter(|path| !path.is_empty())
            } else {
                None
            };
        let Some(id) = id.cloned().or(active_id) else {
            return;
        };
        let tiling_restore_target = if self.floating.has_window(&id) {
            self.inactive_tiling_restore_target()
        } else {
            None
        };

        // Clear floating fullscreen before unfloating.
        if self.floating.is_fullscreen(&id) {
            self.floating.set_fullscreen(&id, false);
        }

        if !explicit_window
            && target_is_active
            && command_context == RouteDomain::Workspace
            && !self.floating.active_command_container_selected()
            && !self.tiling.is_empty()
        {
            if let Some((subtree, rect)) = self.tiling.take_workspace_subtree_for_floating() {
                let focus_id = subtree
                    .tiles()
                    .into_iter()
                    .any(|tile| tile.window().id() == &id)
                    .then_some(id.clone());
                self.floating
                    .add_subtree(subtree, rect, None, true, focus_id.as_ref(), true);
                if let Some(focus_id) = focus_id.as_ref() {
                    self.floating.select_wrapper_for_window(focus_id);
                }
                self.floating_is_active = FloatingActive::Yes;
                self.workspace_focus = WorkspaceFocus::OnContent;
            }
            return;
        }

        // Model rule: if a tiling container is selected (focus-parent semantics),
        // floating toggle targets that selected container even if floating focus mode
        // is currently active.
        if !explicit_window
            && target_is_active
            && command_context == RouteDomain::Tiling
            && self.tiling.selected_is_container()
        {
            let old_parent_ref = self
                .tiling
                .inactive_tiling_reference_for_parent_of_selected_reference();
            if let Some((subtree, origin, rect)) = self.tiling.take_selected_subtree() {
                let focus_id = subtree
                    .tiles()
                    .into_iter()
                    .any(|tile| tile.window().id() == &id)
                    .then_some(id.clone());
                if let Some(reference) = old_parent_ref {
                    if self
                        .tiling
                        .insert_parent_info_from_inactive_tiling_reference(&reference)
                        .is_some()
                    {
                        self.remember_inactive_tiling_reference(reference);
                    }
                }
                self.floating.add_subtree(
                    subtree,
                    rect,
                    origin,
                    target_is_active,
                    focus_id.as_ref(),
                    false,
                );
                if target_is_active {
                    if let Some(focus_id) = focus_id.as_ref() {
                        self.floating.select_wrapper_for_window(focus_id);
                    }
                    self.floating_is_active = FloatingActive::Yes;
                    self.workspace_focus = if self.tiling.is_empty() {
                        WorkspaceFocus::OnWorkspace
                    } else {
                        WorkspaceFocus::OnContent
                    };
                }
            }
            return;
        }

        if self.floating.has_window(&id) {
            // Floating -> Tiling inserts directly using the inactive tiling
            // reference. No tree collapse/normalization.
            if !explicit_window {
                let was_the_workspace = self.floating.active_container_is_workspace_floated();
                if let Some((subtree, origin, _rect)) = self.floating.take_container_subtree(&id) {
                    // Internal implicit single-child split wrappers from
                    // floating must not materialize in tiling.
                    let subtree = subtree.collapse_implicit_single_child_split_root();
                    let tiling_was_empty = self.tiling.is_empty();
                    // When tiling is empty, do not restore against inactive
                    // references/origin; insert directly as workspace tiling root.
                    let restore_info = if tiling_was_empty {
                        None
                    } else {
                        match tiling_restore_target.as_ref() {
                            Some((info, InactiveTilingRestoreSource::Current)) => {
                                origin.as_ref().or(Some(info))
                            }
                            Some((info, _)) => Some(info),
                            None => origin.as_ref(),
                        }
                    };
                    if was_the_workspace && tiling_was_empty {
                        // It was the whole workspace on the way out; it is the whole
                        // workspace on the way back.
                        self.tiling
                            .restore_workspace_subtree(subtree, target_is_active);
                    } else if let Some(info) = restore_info {
                        self.tiling.insert_subtree_with_parent_info(
                            info,
                            subtree,
                            target_is_active,
                        );
                    } else {
                        self.tiling
                            .add_subtree_as_workspace_tiling_fallback(subtree, target_is_active);
                    }

                    if target_is_active {
                        if tiling_was_empty {
                            let _ = self.tiling.activate_window(&id);
                            if let Some(path) = preserve_selection_path_on_unfloat.as_ref() {
                                let _ = self.tiling.select_container_path(path);
                            }
                        }
                        self.floating_is_active = FloatingActive::No;
                        if preserve_workspace_context_on_unfloat && !self.tiling.is_empty() {
                            self.workspace_focus = WorkspaceFocus::OnWorkspace;
                        } else {
                            self.sync_tiling_focus_context_from_tiling();
                        }
                    }
                    return;
                }
            }
        }

        let render_pos = self
            .tiles_with_render_positions()
            .find(|(tile, _, _)| *tile.window().id() == id)
            .map(|(_, pos, _)| pos);

        if self.floating.has_window(&id) {
            // Single window floating → tiling
            let removed = self.floating.remove_tile(&id);
            let mut tile = removed.tile;
            tile.set_scratchpad(false);
            if !self.tiling.is_empty() {
                if let Some((info, _)) = tiling_restore_target.as_ref() {
                    self.tiling.insert_subtree_with_parent_info(
                        info,
                        DetachedNode::Leaf(tile),
                        target_is_active,
                    );
                } else {
                    self.tiling
                        .add_tile_as_workspace_tiling_fallback(tile, target_is_active);
                }
            } else {
                self.tiling
                    .add_tile_as_workspace_tiling_fallback(tile, target_is_active);
            }
            if target_is_active {
                self.floating_is_active = FloatingActive::No;
                if preserve_workspace_context_on_unfloat && !self.tiling.is_empty() {
                    self.workspace_focus = WorkspaceFocus::OnWorkspace;
                } else {
                    self.sync_tiling_focus_context_from_tiling();
                }
            }
        } else {
            // Tiling → Floating
            let old_parent_ref = if target_is_active {
                self.tiling
                    .inactive_tiling_reference_for_parent_of_window(&id)
            } else {
                None
            };
            let mut remembered_old_parent_ref = false;
            let mut removed = self.tiling.remove_tile(&id, Transaction::new());
            if target_is_active {
                if let Some(reference) = old_parent_ref {
                    if self
                        .tiling
                        .insert_parent_info_from_inactive_tiling_reference(&reference)
                        .is_some()
                    {
                        self.remember_inactive_tiling_reference(reference);
                        remembered_old_parent_ref = true;
                    }
                }
            }
            removed.tile.stop_move_animations();
            removed.tile.pending_maximized = false;

            let stored_or_default = self.floating.stored_or_default_tile_pos(&removed.tile);
            if stored_or_default.is_none() {
                removed.tile.floating_pos = None;
                self.assign_default_floating_size_if_missing(&mut removed.tile, true);
            }

            self.floating
                .add_tile_with_restore_hint(removed.tile, target_is_active);
            if target_is_active {
                self.floating_is_active = FloatingActive::Yes;
                self.workspace_focus = WorkspaceFocus::OnContent;
                if !remembered_old_parent_ref && !self.tiling.is_empty() {
                    self.remember_current_tiling_focused_leaf_reference();
                }
            }
        }

        // Animate position transition if possible.
        if let (Some(render_pos), Some((tile, new_render_pos))) = (
            render_pos,
            self.tiles_with_render_positions_mut(false)
                .find(|(tile, _)| *tile.window().id() == id),
        ) {
            tile.animate_move_from(render_pos - new_render_pos);
        }
    }

    pub fn scratchpad_window_id(&self) -> Option<W::Id> {
        self.floating
            .tiles()
            .find(|tile| tile.is_scratchpad())
            .map(|tile| tile.window().id().clone())
    }

    pub fn take_tile_for_scratchpad(&mut self, id: &W::Id) -> Option<Tile<W>> {
        let removed = self.remove_tile(id, Transaction::new());
        let mut tile = removed.tile;
        tile.set_scratchpad(true);
        tile.window_mut().set_floating(true);

        if !removed.is_floating {
            tile.stop_move_animations();
            tile.clear_resize_animation();
            tile.pending_maximized = false;
            // Always center scratchpad windows when first shown.
            tile.floating_pos = None;

            if let Some(size) = self.assign_default_floating_size_if_missing(&mut tile, false) {
                let working_size = self.floating.working_area().size;
                let size_f = Size::from((size.w as f64, size.h as f64));
                let pos = center_preferring_top_left_in_area(self.floating.working_area(), size_f);
                tile.floating_pos = Some(self.floating.logical_to_size_frac(pos));

                let border_config = self
                    .options
                    .layout
                    .border
                    .merged_with(&tile.window().rules().border);
                let bounds = compute_toplevel_bounds(border_config, working_size);
                let win = tile.window_mut();
                win.set_bounds(bounds);
                win.send_pending_configure();
                win.refresh();
            }
        }

        Some(tile)
    }

    pub fn add_scratchpad_tile(&mut self, mut tile: Tile<W>, activate: bool) {
        tile.set_scratchpad(true);
        tile.window_mut().set_floating(true);
        self.enter_output_for_window(tile.window());
        self.floating.add_tile(tile, activate);

        if activate || self.tiling.is_empty() {
            self.floating_is_active = FloatingActive::Yes;
            self.workspace_focus = WorkspaceFocus::OnContent;
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
        if self.floating.is_empty() || self.tiling.is_empty() {
            return;
        }

        self.tiling.clear_selection_context();
        self.floating.clear_selection_context();
        if !self.floating_is_active.get() {
            self.remember_current_tiling_reference();
        }
        self.workspace_focus = WorkspaceFocus::OnContent;
        let was_floating_active = self.floating_is_active.get();
        self.floating_is_active = if was_floating_active {
            FloatingActive::No
        } else {
            FloatingActive::Yes
        };
        if !self.floating_is_active.get() {
            self.sync_tiling_focus_context_from_tiling();
        }
    }

    pub fn clear_selection_context(&mut self) {
        self.tiling.clear_selection_context();
        self.floating.clear_selection_context();
        self.workspace_focus = WorkspaceFocus::OnContent;
    }

    pub fn move_floating_window(
        &mut self,
        id: Option<&W::Id>,
        x: PositionChange,
        y: PositionChange,
        animate: bool,
    ) {
        if self.is_floating_target(id) {
            self.floating.move_window(id, x, y, animate);
        } else {
            // If the target tile isn't floating, set its stored floating position.
            let tile = if let Some(id) = id {
                self.tiling
                    .tiles_mut()
                    .find(|tile| tile.window().id() == id)
                    .unwrap()
            } else if let Some(tile) = self.tiling.active_tile_mut() {
                tile
            } else {
                return;
            };

            let pos = self.floating.stored_or_default_tile_pos(tile);

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

            let working_area = self.floating.working_area();
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

            let pos = self.floating.logical_to_size_frac(pos);
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
        let tiling = self.tiling.tiles_with_render_positions();

        let floating = self.floating.tiles_with_render_positions();
        let visible = self.is_floating_visible();
        let floating = floating.map(move |(tile, pos)| (tile, pos, visible));

        floating.chain(tiling)
    }

    pub fn tiles_with_render_positions_mut(
        &mut self,
        round: bool,
    ) -> impl Iterator<Item = (&mut Tile<W>, Point<f64, Logical>)> {
        let tiling = self.tiling.tiles_with_render_positions_mut(round);
        let floating = self.floating.tiles_with_render_positions_mut(round);
        floating.chain(tiling)
    }

    pub fn tiles_with_ipc_layouts(&self) -> impl Iterator<Item = (&Tile<W>, WindowLayout)> {
        let tiling = self.tiling.tiles_with_ipc_layouts();
        let floating = self.floating.tiles_with_ipc_layouts();
        floating.chain(tiling)
    }

    pub fn active_window_visual_rectangle(&self) -> Option<Rectangle<f64, Logical>> {
        if self.floating_is_active.get() {
            self.floating.active_window_visual_rectangle()
        } else {
            self.tiling.active_tile_visual_rectangle()
        }
    }

    pub fn popup_target_rect(&self, window: &W::Id) -> Option<Rectangle<f64, Logical>> {
        if self.floating.has_window(window) {
            self.floating.popup_target_rect(window)
        } else {
            self.tiling.popup_target_rect(window)
        }
    }

    pub fn render_tiling<R: NiriRenderer>(
        &self,
        ctx: RenderCtx<R>,
        xray_pos: XrayPos,
        focus_ring: bool,
        push: &mut dyn FnMut(WorkspaceRenderElement<R>),
    ) {
        let tiling_focus_ring = focus_ring && !self.floating_is_active();
        self.tiling
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
        let tiling_focus_ring = focus_ring && !self.floating_is_active();
        if let Some(elem) = self
            .tiling
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
        self.floating
            .render(ctx, xray_pos, view_rect, floating_focus_ring, &mut |elem| {
                push(elem.into())
            });
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
        self.tiling.render_above_top_layer()
    }

    pub fn is_floating_visible(&self) -> bool {
        // If the focus is on a fullscreen tiling window, hide the floating windows.
        matches!(
            self.floating_is_active,
            FloatingActive::Yes | FloatingActive::NoButRaised
        ) || !self.render_above_top_layer()
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
        if self.floating.has_window(window) {
            self.floating
                .start_close_animation_for_window(renderer, window, blocker);
        } else {
            self.tiling
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
        self.floating
            .start_close_animation_for_tile(renderer, snapshot, tile_size, tile_pos, blocker);
    }

    pub fn start_open_animation(&mut self, id: &W::Id) -> bool {
        self.tiling.start_open_animation(id) || self.floating.start_open_animation(id)
    }

    pub fn window_under(&self, pos: Point<f64, Logical>) -> Option<(&W, HitType)> {
        if self.is_floating_visible() {
            if let Some(rv) = self.floating.window_under(pos) {
                return Some(rv);
            }
        }

        self.tiling.window_under(pos)
    }

    pub fn resize_edges_under(&mut self, pos: Point<f64, Logical>) -> Option<ResizeEdge> {
        self.resize_hit_under(pos).map(|hit| hit.edges)
    }

    pub fn resize_hit_under(&mut self, pos: Point<f64, Logical>) -> Option<ResizeHit<W::Id>> {
        if self.is_active_pending_fullscreen() {
            return None;
        }

        if self.is_floating_visible() {
            match self.floating.resize_hit_under(pos) {
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

        self.tiling.resize_hit_under(pos)
    }

    pub fn descendants_added(&mut self, id: &W::Id) -> bool {
        self.floating.descendants_added(id)
    }

    pub fn update_window(&mut self, window: &W::Id, serial: Option<Serial>) {
        if !self.floating.update_window(window, serial) {
            self.tiling.update_window(window, serial);
        }
    }

    pub fn refresh(&mut self, is_active: bool, is_focused: bool) {
        self.tiling
            .refresh(is_active && !self.floating_is_active.get(), is_focused);
        self.floating
            .refresh(is_active && self.floating_is_active.get(), is_focused);
    }

    pub fn activation_view_distance(&self, window: &W::Id) -> f64 {
        if self.floating.has_window(window) {
            return 0.;
        }

        self.tiling.activation_view_distance(window)
    }

    pub fn is_urgent(&self) -> bool {
        self.windows().any(|win| win.is_urgent())
    }

    pub fn activate_window(&mut self, window: &W::Id) -> bool {
        if self.floating.activate_window(window) {
            self.floating_is_active = FloatingActive::Yes;
            self.workspace_focus = WorkspaceFocus::OnContent;
            true
        } else if self.tiling.activate_window(window) {
            self.floating_is_active = FloatingActive::No;
            self.sync_tiling_focus_context_from_tiling();
            true
        } else {
            false
        }
    }

    pub fn activate_window_without_raising(&mut self, window: &W::Id) -> bool {
        if self.floating.activate_window_without_raising(window) {
            self.floating_is_active = FloatingActive::Yes;
            self.workspace_focus = WorkspaceFocus::OnContent;
            true
        } else if self.tiling.activate_window(window) {
            self.floating_is_active = match self.floating_is_active {
                FloatingActive::No => FloatingActive::No,
                FloatingActive::NoButRaised => FloatingActive::NoButRaised,
                FloatingActive::Yes => FloatingActive::NoButRaised,
            };
            self.sync_tiling_focus_context_from_tiling();
            true
        } else {
            false
        }
    }

    pub(super) fn tiling_insert_position(&self, pos: Point<f64, Logical>) -> InsertPosition {
        self.tiling.insert_position(pos)
    }

    pub(super) fn insert_hint_area(
        &self,
        position: &InsertPosition,
    ) -> Option<Rectangle<f64, Logical>> {
        self.tiling.insert_hint_area(position)
    }

    pub fn horizontal_view_gesture_begin(&mut self, is_touchpad: bool) {
        self.tiling.horizontal_view_gesture_begin(is_touchpad);
    }

    pub fn horizontal_view_gesture_update(
        &mut self,
        delta_x: f64,
        timestamp: Duration,
        is_touchpad: bool,
    ) -> Option<bool> {
        self.tiling
            .horizontal_view_gesture_update(delta_x, timestamp, is_touchpad)
    }

    pub fn horizontal_view_gesture_end(&mut self, is_touchpad: Option<bool>) -> bool {
        self.tiling.horizontal_view_gesture_end(is_touchpad)
    }

    pub fn interactive_resize_begin(&mut self, window: W::Id, edges: ResizeEdge) -> bool {
        if self.floating.has_window(&window) {
            self.floating.interactive_resize_begin(window, edges)
        } else {
            self.tiling.interactive_resize_begin(window, edges)
        }
    }

    pub fn interactive_resize_begin_at(
        &mut self,
        window: W::Id,
        edges: ResizeEdge,
        pos: Point<f64, Logical>,
    ) -> bool {
        if self.floating.has_window(&window) {
            self.floating.interactive_resize_begin(window, edges)
        } else {
            self.tiling.interactive_resize_begin_at(window, edges, pos)
        }
    }

    pub fn interactive_resize_update(
        &mut self,
        window: &W::Id,
        delta: Point<f64, Logical>,
    ) -> bool {
        if self.floating.has_window(window) {
            self.floating.interactive_resize_update(window, delta)
        } else {
            self.tiling.interactive_resize_update(window, delta)
        }
    }

    pub fn interactive_resize_end(&mut self, window: Option<&W::Id>) {
        if let Some(window) = window {
            if self.floating.has_window(window) {
                self.floating.interactive_resize_end(Some(window));
            } else {
                self.tiling.interactive_resize_end(Some(window));
            }
        } else {
            self.floating.interactive_resize_end(None);
            self.tiling.interactive_resize_end(None);
        }
    }

    pub fn floating_is_active(&self) -> bool {
        self.floating_is_active.get()
    }

    pub fn floating_logical_to_size_frac(
        &self,
        logical_pos: Point<f64, Logical>,
    ) -> Point<f64, SizeFrac> {
        self.floating.logical_to_size_frac(logical_pos)
    }

    pub(super) fn floating_container_allows_splits(&self, id: &W::Id) -> bool {
        self.floating.container_allows_splits(id)
    }

    pub(super) fn floating_container_pos(&self, id: &W::Id) -> Option<Point<f64, Logical>> {
        self.floating.container_pos(id)
    }

    pub(super) fn move_floating_container_for_window_to(
        &mut self,
        id: &W::Id,
        pos: Point<f64, Logical>,
    ) -> bool {
        self.floating.move_container_for_window_to(id, pos, false)
    }

    pub fn working_area(&self) -> Rectangle<f64, Logical> {
        self.working_area
    }

    pub fn layout_config(&self) -> Option<&tiri_config::LayoutPart> {
        self.layout_config.as_ref()
    }

    #[cfg(test)]
    pub fn tiling(&self) -> &TilingSpace<W> {
        &self.tiling
    }

    #[cfg(test)]
    pub fn floating(&self) -> &FloatingSpace<W> {
        &self.floating
    }

    #[cfg(test)]
    pub fn debug_inactive_tiling_focus_stack(&self) -> Vec<String> {
        self.inactive_tiling_focus_stack
            .iter()
            .map(|reference| format!("{reference:?}"))
            .collect()
    }

    #[cfg(test)]
    pub fn debug_active_floating_wrapper_selected(&self) -> bool {
        self.floating.active_wrapper_selected()
    }

    #[cfg(test)]
    pub fn debug_active_floating_container_allows_splits(&self) -> bool {
        self.floating.active_container_allows_splits()
    }

    #[cfg(test)]
    pub fn debug_active_floating_command_container_path(&self) -> Option<Vec<usize>> {
        self.floating.active_command_container_path()
    }

    #[cfg(test)]
    pub fn debug_command_context(&self) -> &'static str {
        match self.resolved_command_route().default_domain {
            RouteDomain::Workspace => "workspace",
            RouteDomain::Tiling => "tiling",
            RouteDomain::Floating => "floating",
        }
    }

    #[cfg(test)]
    pub fn debug_command_target(&self) -> &'static str {
        match self.resolved_command_route().command_target {
            CommandTarget::Workspace => "workspace",
            CommandTarget::TilingWindow => "tiling_window",
            CommandTarget::TilingContainer => "tiling_container",
            CommandTarget::FloatingWindow => "floating_window",
            CommandTarget::FloatingContainer => "floating_container",
        }
    }

    #[cfg(test)]
    pub fn debug_route_domain_for_focus(&self) -> &'static str {
        match self.route_domain_for_family(CommandFamily::Focus) {
            RouteDomain::Workspace => "workspace",
            RouteDomain::Tiling => "tiling",
            RouteDomain::Floating => "floating",
        }
    }

    #[cfg(test)]
    pub fn debug_route_domain_for_layout(&self) -> &'static str {
        match self.route_domain_for_family(CommandFamily::Layout) {
            RouteDomain::Workspace => "workspace",
            RouteDomain::Tiling => "tiling",
            RouteDomain::Floating => "floating",
        }
    }

    #[cfg(test)]
    pub fn debug_route_domain_for_move_directional(&self) -> &'static str {
        match self.route_domain_for_family(CommandFamily::MoveDirectional) {
            RouteDomain::Workspace => "workspace",
            RouteDomain::Tiling => "tiling",
            RouteDomain::Floating => "floating",
        }
    }

    #[cfg(test)]
    pub fn debug_route_domain_for_move_container(&self) -> &'static str {
        match self.route_domain_for_family(CommandFamily::MoveContainer) {
            RouteDomain::Workspace => "workspace",
            RouteDomain::Tiling => "tiling",
            RouteDomain::Floating => "floating",
        }
    }

    #[cfg(test)]
    pub fn debug_floating_workspace_context(&self) -> bool {
        self.is_floating_workspace_context_active()
    }

    #[cfg(test)]
    pub fn debug_workspace_layout(&self) -> Layout {
        self.tiling.debug_workspace_layout()
    }

    #[cfg(test)]
    pub fn debug_inactive_tiling_restore_target(&mut self) -> Option<String> {
        self.inactive_tiling_restore_target()
            .map(|(info, source)| format!("{source:?} {info:?}"))
    }

    #[cfg(test)]
    pub fn verify_invariants(&self, move_win_id: Option<&W::Id>) {
        use approx::assert_abs_diff_eq;

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

        // The workspace command context invariants that used to be asserted here — never both
        // floating and tiling elevated, and elevation matching the active focus mode — are now
        // enforced by construction: `workspace_focus` is a single elevation bit and the active
        // layer is always derived from `floating_is_active`. See `WorkspaceFocus`.

        assert_eq!(self.view_size, self.tiling.view_size());
        assert_eq!(self.working_area, self.tiling.parent_area());
        assert_eq!(&self.clock, self.tiling.clock());
        assert!(Rc::ptr_eq(&self.options, self.tiling.options()));
        self.tiling.verify_invariants();

        assert_eq!(self.view_size, self.floating.view_size());
        assert_eq!(self.working_area, self.floating.working_area());
        assert_eq!(&self.clock, self.floating.clock());
        assert!(Rc::ptr_eq(&self.options, self.floating.options()));
        self.floating.verify_invariants();

        if self.floating.is_empty() {
            assert!(
                !self.floating_is_active.get(),
                "when floating is empty it must never be active"
            );
        } else if self.tiling.is_empty() {
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
            self.tiling.layout_tree_unfocused()
        } else {
            self.tiling.layout_tree()
        }
    }

    pub(crate) fn floating_layout_tree_nodes(&self) -> Vec<LayoutTreeNode> {
        self.floating.layout_tree_nodes()
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
