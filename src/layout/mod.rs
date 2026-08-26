//! Window layout logic.
//!
//! Niri implements i3-style hierarchical tiling with dynamic workspaces. The tiling system is mostly
//! orthogonal to any particular workspace system, though outputs living in separate coordinate
//! spaces suggest per-output workspaces.
//!
//! I chose a dynamic workspace system because I think it works very well. In particular, it works
//! naturally across outputs getting added and removed, since workspaces can move between outputs
//! as necessary.
//!
//! In the layout, one output (the first one to be added) is designated as *primary*. This is where
//! workspaces from disconnected outputs will move. Currently, the primary output has no other
//! distinction from other outputs.
//!
//! Where possible, niri tries to follow these principles with regards to outputs:
//!
//! 1. Disconnecting and reconnecting the same output must not change the layout.
//!    * This includes both secondary outputs and the primary output.
//! 2. Connecting an output must not change the layout for any workspaces that were never on that
//!    output.
//!
//! Therefore, we implement the following logic: every workspace keeps track of which output it
//! originated on—its *original output*. When an output disconnects, its workspaces are appended to
//! the (potentially new) primary output, but remember their original output. Then, if the original
//! output connects again, all workspaces originally from there move back to that output.
//!
//! In order to avoid surprising behavior, if the user creates or moves any new windows onto a
//! workspace, it forgets its original output, and its current output becomes its original output.
//! Imagine a scenario: the user works with a laptop and a monitor at home, then takes their laptop
//! with them, disconnecting the monitor, and keeps working as normal, using the second monitor's
//! workspace just like any other. Then they come back, reconnect the second monitor, and now we
//! don't want an unassuming workspace to end up on it.

use std::collections::HashMap;
use std::mem;
use std::rc::Rc;
use std::time::Duration;

use container_tree::RootTilingSubtree;
use legacy_column::{Column, ColumnWidth};
use monitor::{InsertHint, InsertPosition, InsertWorkspace, MonitorAddWindowTarget};
use smithay::backend::renderer::element::surface::WaylandSurfaceRenderElement;
use smithay::backend::renderer::element::utils::RescaleRenderElement;
use smithay::backend::renderer::gles::{GlesRenderer, GlesTexture};
use smithay::input::pointer::CursorIcon;
use smithay::output::{self, Output};
use smithay::reexports::wayland_server::protocol::wl_surface::WlSurface;
use smithay::utils::{Logical, Point, Rectangle, Scale, Serial, Size, Transform};
use tile::{Tile, TileRenderElement};
use tiri_config::utils::MergeWith as _;
use tiri_config::{
    Config, CornerRadius, LayoutPart, PresetSize, Workspace as WorkspaceConfig, WorkspaceReference,
};
use tiri_ipc::{ColumnDisplay, LayoutTree, PositionChange, SizeChange, WindowLayout};
use workspace::{WorkspaceAddWindowTarget, WorkspaceId, WorkspaceLifetime};

pub use self::container::{Direction, Layout as ContainerLayout};
use self::container::{InsertParentInfo, NodeKey};
pub use self::monitor::MonitorRenderElement;
use self::monitor::{Monitor, WorkspaceSwitch};
use self::seat_focus::{SeatFocusNode, SeatFocusStack};
use self::workspace::{OutputId, Workspace};
use crate::animation::{Animation, Clock};
use crate::input::swipe_tracker::SwipeTracker;
use crate::niri_render_elements;
use crate::render_helpers::background_effect::BackgroundEffectElement;
use crate::render_helpers::offscreen::OffscreenData;
use crate::render_helpers::renderer::NiriRenderer;
use crate::render_helpers::snapshot::RenderSnapshot;
use crate::render_helpers::solid_color::{SolidColorBuffer, SolidColorRenderElement};
use crate::render_helpers::texture::TextureBuffer;
use crate::render_helpers::xray::{Xray, XrayPos};
use crate::render_helpers::{BakedBuffer, RenderCtx};
use crate::rubber_band::RubberBand;
use crate::utils::transaction::{Transaction, TransactionBlocker};
use crate::utils::{
    ensure_min_max_size_maybe_zero, output_matches_name, output_size,
    round_logical_in_physical_max1, ResizeEdge,
};
use crate::window::ResolvedWindowRules;

/// One entry in sway's explicit `layout toggle ...` cycle.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum LayoutCycleEntry {
    /// Match either split orientation and switch orientation when this entry is selected.
    Split,
    /// Match and select one concrete layout.
    Layout(ContainerLayout),
}

pub mod closing_window;
pub mod container;
pub mod container_tree;
pub mod floating;
pub mod focus_ring;
pub mod insert_hint_element;
pub mod legacy_column;
pub mod monitor;
pub mod opening_window;
mod seat_focus;
pub mod shadow;
pub mod tab_bar;
pub mod tab_indicator;
pub mod tile;
pub mod tiling_space;
mod viewport;
pub mod workspace;

#[cfg(test)]
mod tests;

/// Size changes up to this many pixels don't animate.
pub const RESIZE_ANIMATION_THRESHOLD: f64 = 10.;

/// Axis selected by a non-directional resize command.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ResizeAxis {
    Horizontal,
    Vertical,
}

/// A complete keyboard/IPC resize request.
///
/// Axis requests can set or adjust a size and, for tiled windows, share the change across all
/// siblings. Edge requests are signed pixel adjustments and move only the named edge.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum ResizeRequest {
    Axis {
        axis: ResizeAxis,
        change: SizeChange,
    },
    Edge {
        direction: Direction,
        amount: i32,
    },
}

impl ResizeRequest {
    pub(crate) fn axis(self) -> ResizeAxis {
        match self {
            Self::Axis { axis, .. } => axis,
            Self::Edge { direction, .. } => match direction {
                Direction::Left | Direction::Right => ResizeAxis::Horizontal,
                Direction::Up | Direction::Down => ResizeAxis::Vertical,
            },
        }
    }
}

/// Pointer distance to count as a resize edge.
const RESIZE_EDGE_THRESHOLD: f64 = 10.;

/// Pointer needs to move this far to pull a window from the layout.
const INTERACTIVE_MOVE_START_THRESHOLD: f64 = 256. * 256.;

/// Opacity of interactively moved tiles targeting the tiling layout.
const INTERACTIVE_MOVE_ALPHA: f64 = 0.75;

/// Amount of touchpad movement to toggle the overview.
const OVERVIEW_GESTURE_MOVEMENT: f64 = 300.;

const OVERVIEW_GESTURE_RUBBER_BAND: RubberBand = RubberBand {
    stiffness: 0.5,
    limit: 0.05,
};

/// Size-relative units.
pub struct SizeFrac;

niri_render_elements! {
    LayoutElementRenderElement<R> => {
        Wayland = WaylandSurfaceRenderElement<R>,
        SolidColor = SolidColorRenderElement,
        BackgroundEffect = BackgroundEffectElement,
    }
}

fn resize_edges_for_point(
    pos: Point<f64, Logical>,
    size: Size<f64, Logical>,
    border_width: Option<f64>,
) -> ResizeEdge {
    let border = border_width.unwrap_or(0.0) * 2.0;
    let threshold = RESIZE_EDGE_THRESHOLD.max(border);
    let threshold_x = threshold.min(size.w / 2.0);
    let threshold_y = threshold.min(size.h / 2.0);

    let mut edges = ResizeEdge::empty();
    if pos.x <= threshold_x {
        edges |= ResizeEdge::LEFT;
    } else if pos.x >= size.w - threshold_x {
        edges |= ResizeEdge::RIGHT;
    }
    if pos.y <= threshold_y {
        edges |= ResizeEdge::TOP;
    } else if pos.y >= size.h - threshold_y {
        edges |= ResizeEdge::BOTTOM;
    }
    edges
}

pub type LayoutElementRenderSnapshot =
    RenderSnapshot<BakedBuffer<TextureBuffer<GlesTexture>>, BakedBuffer<SolidColorBuffer>>;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SizingMode {
    Normal,
    Fullscreen,
}

pub trait LayoutElement {
    /// Type that can be used as a unique ID of this element.
    type Id: PartialEq + std::fmt::Debug + Clone;

    /// Unique ID of this element.
    fn id(&self) -> &Self::Id;

    /// Numeric identity of this element as exposed over IPC.
    ///
    /// `Id` is whatever an implementation finds convenient internally; this is the one every
    /// consumer outside the compositor sees, so it must be stable for the element's lifetime.
    fn ipc_id(&self) -> u64;

    /// Optional window title for UI elements like tab bars.
    fn title(&self) -> Option<String> {
        None
    }

    /// Optional application id, for IPC.
    fn app_id(&self) -> Option<String> {
        None
    }

    /// Optional pid of the process owning this element, for IPC.
    fn pid(&self) -> Option<i32> {
        None
    }

    /// Updates the config for the element.
    fn update_config(&mut self, blur_config: tiri_config::Blur) {
        let _ = blur_config;
    }

    /// Visual size of the element.
    ///
    /// This is what the user would consider the size, i.e. excluding CSD shadows and whatnot.
    /// Corresponds to the Wayland window geometry size.
    fn size(&self) -> Size<i32, Logical>;

    /// The size the element asked for when it mapped, before any layout stretched it.
    ///
    /// sway's `view->natural_width/height`, set once from the toplevel's geometry and never
    /// updated. It is what a window gets when it starts floating, so it has to survive the
    /// window having been tiled at some other size in between — which is exactly what
    /// [`Self::size`] cannot tell you.
    fn natural_size(&self) -> Size<i32, Logical>;

    /// Returns the location of the element's buffer relative to the element's visual geometry.
    ///
    /// I.e. if the element has CSD shadows, its buffer location will have negative coordinates.
    fn buf_loc(&self) -> Point<i32, Logical>;

    /// Checks whether a point is in the element's input region.
    ///
    /// The point is relative to the element's visual geometry.
    fn is_in_input_region(&self, point: Point<f64, Logical>) -> bool;

    /// Renders the element at the given visual location.
    ///
    /// The element should be rendered in such a way that its visual geometry ends up at the given
    /// location.
    fn render<R: NiriRenderer>(
        &self,
        mut ctx: RenderCtx<R>,
        location: Point<f64, Logical>,
        scale: Scale<f64>,
        alpha: f32,
        xray_pos: XrayPos,
        push: &mut dyn FnMut(LayoutElementRenderElement<R>),
    ) {
        self.render_popups(ctx.r(), location, scale, alpha, xray_pos, push);
        self.render_normal(ctx.r(), location, scale, alpha, push);
    }

    /// Renders the non-popup parts of the element.
    fn render_normal<R: NiriRenderer>(
        &self,
        ctx: RenderCtx<R>,
        location: Point<f64, Logical>,
        scale: Scale<f64>,
        alpha: f32,
        push: &mut dyn FnMut(LayoutElementRenderElement<R>),
    ) {
        let _ = (ctx, location, scale, alpha, push);
    }

    /// Renders the popups of the element.
    fn render_popups<R: NiriRenderer>(
        &self,
        ctx: RenderCtx<R>,
        location: Point<f64, Logical>,
        scale: Scale<f64>,
        alpha: f32,
        xray_pos: XrayPos,
        push: &mut dyn FnMut(LayoutElementRenderElement<R>),
    ) {
        let _ = (ctx, location, scale, alpha, xray_pos, push);
    }

    /// Renders the background effect behind the main surface of the element.
    #[allow(clippy::too_many_arguments)]
    fn render_background_effect(
        &self,
        _ctx: RenderCtx<GlesRenderer>,
        _geometry: Rectangle<f64, Logical>,
        _scale: f64,
        _clip_to_geometry: bool,
        _surface_anim_scale: Scale<f64>,
        _radius: CornerRadius,
        _xray_pos: XrayPos,
        _push: &mut dyn FnMut(BackgroundEffectElement),
    ) {
    }

    /// Requests the element to change its size.
    ///
    /// The size request is stored and will be continuously sent to the element on any further
    /// state changes.
    fn request_size(
        &mut self,
        size: Size<i32, Logical>,
        mode: SizingMode,
        animate: bool,
        transaction: Option<Transaction>,
    );

    /// Requests the element to change size once, clearing the request afterwards.
    fn request_size_once(&mut self, size: Size<i32, Logical>, animate: bool) {
        self.request_size(size, SizingMode::Normal, animate, None);
    }

    fn min_size(&self) -> Size<i32, Logical>;
    fn max_size(&self) -> Size<i32, Logical>;
    fn is_wl_surface(&self, wl_surface: &WlSurface) -> bool;
    fn has_ssd(&self) -> bool;
    fn set_preferred_scale_transform(&self, scale: output::Scale, transform: Transform);
    fn output_enter(&self, output: &Output);
    fn output_leave(&self, output: &Output);
    fn set_offscreen_data(&self, data: Option<OffscreenData>);
    fn set_activated(&mut self, active: bool);
    fn set_active_in_column(&mut self, active: bool);
    fn set_floating(&mut self, floating: bool);
    fn set_bounds(&self, bounds: Size<i32, Logical>);
    fn is_ignoring_opacity_window_rule(&self) -> bool;

    fn is_urgent(&self) -> bool;

    fn configure_intent(&self) -> ConfigureIntent;
    fn send_pending_configure(&mut self);

    /// The element's current sizing mode.
    ///
    /// This will *not* switch immediately after a [`LayoutElement::request_size()`] call.
    fn sizing_mode(&self) -> SizingMode;

    /// The sizing mode that we're requesting the element to assume.
    ///
    /// This *will* switch immediately after a [`LayoutElement::request_size()`] call.
    fn pending_sizing_mode(&self) -> SizingMode;

    /// Size previously requested through [`LayoutElement::request_size()`].
    fn requested_size(&self) -> Option<Size<i32, Logical>>;

    /// Non-fullscreen size that we expect this window has or will shortly have.
    ///
    /// This can be different from [`requested_size()`](LayoutElement::requested_size()). For
    /// example, for floating windows this will generally return the current window size, rather
    /// than the last size that we requested, since we want floating windows to be able to change
    /// size freely. But not always: if we just requested a floating window to resize and it hasn't
    /// responded to it yet, this will return the newly requested size.
    ///
    /// This function should never return a 0 size component. `None` means there's no known
    /// expected size (for example, the window is fullscreen).
    ///
    /// The default impl is for testing only, it will not preserve the window's own size changes.
    fn expected_size(&self) -> Option<Size<i32, Logical>> {
        if self.sizing_mode().is_fullscreen() {
            return None;
        }

        let mut requested = self.requested_size().unwrap_or_default();
        let current = self.size();
        if requested.w == 0 {
            requested.w = current.w;
        }
        if requested.h == 0 {
            requested.h = current.h;
        }
        Some(requested)
    }

    fn is_windowed_fullscreen(&self) -> bool {
        false
    }
    fn is_pending_windowed_fullscreen(&self) -> bool {
        false
    }
    fn request_windowed_fullscreen(&mut self, value: bool) {
        let _ = value;
    }

    /// The effective geometry corner radius for this element.
    ///
    /// Returns zero when the element is in windowed fullscreen, since fullscreen windows have
    /// square corners.
    ///
    /// This method only handles windowed fullscreen and not maximized/real fullscreen. This is
    /// because windowed fullscreen is handled by the element itself, whereas other sizing modes
    /// are handled externally by the Tile, so the corner radius changes for those modes is also
    /// handled externally.
    fn geometry_corner_radius(&self) -> CornerRadius {
        let rules = self.rules();

        // When windows think they're fullscreen, they square their corners.
        //
        // However, if the user is clipping the window to geometry, they are likely going for
        // consistent corner radius, and want this radius to remain in windowed fullscreen.
        if self.is_windowed_fullscreen() && rules.clip_to_geometry != Some(true) {
            return CornerRadius::default();
        }

        rules.geometry_corner_radius.unwrap_or_default()
    }

    fn is_child_of(&self, parent: &Self) -> bool;

    fn rules(&self) -> &ResolvedWindowRules;

    /// Runs periodic clean-up tasks.
    fn refresh(&self);

    fn take_animation_snapshot(&mut self) -> Option<LayoutElementRenderSnapshot>;

    fn set_interactive_resize(&mut self, data: Option<InteractiveResizeData>);
    fn cancel_interactive_resize(&mut self);
    fn interactive_resize_data(&self) -> Option<InteractiveResizeData>;

    fn on_commit(&mut self, serial: Serial);
}

#[derive(Debug)]
pub struct Layout<W: LayoutElement> {
    /// Monitors and workspaes in the layout.
    monitor_set: MonitorSet<W>,
    /// Whether the layout should draw as active.
    ///
    /// This normally indicates that the layout has keyboard focus, but not always. E.g. when the
    /// screenshot UI is open, it keeps the layout drawing as active.
    is_active: bool,
    /// Map from monitor name to id of its last active workspace.
    ///
    /// This data is stored upon monitor removal and is used to restore the active workspace when
    /// the monitor is reconnected.
    ///
    /// The workspace id does not necessarily point to a valid workspace. If it doesn't, then it is
    /// simply ignored.
    last_active_workspace_id: HashMap<String, WorkspaceId>,
    /// MRU of workspaces and sticky windows across outputs.
    ///
    /// Window/container focus inside a workspace belongs exclusively to its `ContainerTree`
    /// seat. This history only chooses the workspace (or sticky layer) when an output becomes
    /// active, so it cannot disagree with a workspace about its active tiling/floating node.
    seat_focus: SeatFocusStack<W::Id>,
    /// Ongoing interactive move.
    interactive_move: Option<InteractiveMoveState<W>>,
    /// Ongoing drag-and-drop operation.
    dnd: Option<DndData<W>>,
    /// Clock for driving animations.
    clock: Clock,
    /// Time that we last updated render elements for.
    update_render_elements_time: Duration,
    /// Whether the overview is open.
    ///
    /// This is a boolean flag that controls things like where input goes to. The actual animation
    /// is controlled by overview_progress.
    overview_open: bool,
    /// The overview zoom progress.
    overview_progress: Option<OverviewProgress>,
    /// The scratchpad: a workspace that is never on an output.
    ///
    /// sway's scratchpad is `__i3_scratch`, an ordinary workspace on a hidden output, and a
    /// window in it is a window on a workspace — laid out, configured, and in step with its
    /// client like any other. Showing one is `move to workspace`, and there is no third state
    /// for a window to be in.
    ///
    /// Tiri kept a queue of detached tiles instead. A detached tile is arranged by nobody, so
    /// it had no box while hidden and needed a full resize handshake with an idle client on
    /// the way back — which is what made showing one take the whole transaction deadline.
    /// Everything that walks the layout also had to remember the queue was there.
    scratchpad: Workspace<W>,
    /// Configurable properties of the layout.
    options: Rc<Options>,
    /// A semantic layout mutation happened since the compositor last refreshed projections.
    ///
    /// Niri also classifies its public action/protocol entry points, but this bit is the
    /// authoritative signal for callers that intentionally operate on Layout directly.
    refresh_requested: bool,
}

#[derive(Debug)]
enum MonitorSet<W: LayoutElement> {
    /// At least one output is connected.
    Normal {
        /// Connected monitors.
        monitors: Vec<Monitor<W>>,
        /// Index of the primary monitor.
        primary_idx: usize,
        /// Index of the active monitor.
        active_monitor_idx: usize,
    },
    /// No outputs are connected, and these are the workspaces.
    NoOutputs {
        /// The workspaces.
        workspaces: Vec<Workspace<W>>,
    },
}

fn ensure_no_outputs_workspace_idx<W: LayoutElement>(
    workspaces: &mut Vec<Workspace<W>>,
    clock: Clock,
    options: Rc<Options>,
) -> usize {
    if let Some(idx) = workspaces
        .iter()
        .position(|ws| !ws.has_windows_or_persistent_identity())
    {
        idx
    } else {
        workspaces.push(Workspace::new_no_outputs(clock, options));
        workspaces.len() - 1
    }
}

#[derive(Debug, Default, Clone, PartialEq)]
pub struct Options {
    pub layout: tiri_config::Layout,
    pub animations: tiri_config::Animations,
    pub gestures: tiri_config::Gestures,
    pub overview: tiri_config::Overview,
    pub blur: tiri_config::Blur,
    // Debug flags.
    pub disable_resize_throttling: bool,
    pub deactivate_unfocused_windows: bool,
}

/// Which layer owns a window that is being interactively moved.
///
/// Sticky windows belong to a monitor and regular ones to a workspace; both answer the same
/// questions during a drag through different APIs.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum MoveHost {
    /// The sticky layer of the monitor at this index.
    Sticky(usize),
    /// The workspace that holds the window.
    Workspace,
}

#[allow(clippy::large_enum_variant)]
#[derive(Debug)]
enum InteractiveMoveState<W: LayoutElement> {
    /// Initial rubberbanding; the window remains in the layout.
    Starting {
        /// The window we're moving.
        window_id: W::Id,
        /// Current pointer delta from the starting location.
        pointer_delta: Point<f64, Logical>,
        /// Pointer location within the visual window geometry as ratio from geometry size.
        ///
        /// This helps the pointer remain inside the window as it resizes.
        pointer_ratio_within_window: (f64, f64),
    },
    /// Moving; the window is no longer in the layout.
    Moving(InteractiveMoveData<W>),
    /// Moving a floating container; the windows remain in the layout.
    MovingContainer(InteractiveMoveContainerData<W>),
}

#[derive(Debug)]
struct InteractiveMoveData<W: LayoutElement> {
    /// The window being moved.
    pub(self) tile: Tile<W>,
    /// Output where the window is currently located/rendered.
    pub(self) output: Output,
    /// Current pointer position within output.
    pub(self) pointer_pos_within_output: Point<f64, Logical>,
    /// Width of the root tiling subtree the window came from.
    pub(self) width: ColumnWidth,
    /// Whether the window targets the floating layout.
    pub(self) is_floating: bool,
    /// Whether the window was sticky before the move started.
    pub(self) was_sticky: bool,
    /// Pointer location within the visual window geometry as ratio from geometry size.
    ///
    /// This helps the pointer remain inside the window as it resizes.
    pub(self) pointer_ratio_within_window: (f64, f64),
    /// Config overrides for the output where the window is currently located.
    ///
    /// Cached here to be accessible while an output is removed.
    pub(self) output_config: Option<tiri_config::LayoutPart>,
    /// Config overrides for the workspace where the window is currently located.
    ///
    /// To avoid sudden window changes when starting an interactive move, it will remember the
    /// config overrides for the workspace where the move originated from. As soon as the window
    /// moves over some different workspace though, this override will reset.
    pub(self) workspace_config: Option<(WorkspaceId, tiri_config::LayoutPart)>,
    /// Original insert location for swaps within the tiling layout.
    pub(self) swap_origin: Option<InsertParentInfo>,
    /// Workspace where the move originated.
    pub(self) origin_workspace: WorkspaceId,
}

#[derive(Debug)]
struct InteractiveMoveContainerData<W: LayoutElement> {
    /// The window being moved (used to find its container).
    pub(self) window_id: W::Id,
    /// Output where the pointer is currently located.
    pub(self) output: Output,
    /// Current pointer position within output.
    pub(self) pointer_pos_within_output: Point<f64, Logical>,
    /// Pointer position at the start of the container move.
    pub(self) start_pointer_pos_within_output: Point<f64, Logical>,
    /// Container position at the start of the move (workspace coords).
    pub(self) start_container_pos: Point<f64, Logical>,
}

#[derive(Debug)]
pub struct DndData<W: LayoutElement> {
    /// Output where the pointer is currently located.
    output: Output,
    /// Current pointer position within output.
    pointer_pos_within_output: Point<f64, Logical>,
    /// Ongoing DnD hold to activate something.
    hold: Option<DndHold<W>>,
}

#[derive(Debug)]
struct DndHold<W: LayoutElement> {
    /// Time when we started holding on the target.
    start_time: Duration,
    target: DndHoldTarget<W::Id>,
}

#[derive(Debug, PartialEq, Eq)]
enum DndHoldTarget<WindowId> {
    Window(WindowId),
    Workspace(WorkspaceId),
}

#[derive(Debug, Clone, Copy)]
pub struct InteractiveResizeData {
    pub(self) edges: ResizeEdge,
}

#[derive(Debug, Clone)]
pub struct ResizeHit<WId> {
    pub window: WId,
    pub edges: ResizeEdge,
    pub cursor: CursorIcon,
    pub is_floating: bool,
}

#[derive(Debug, Clone, Copy)]
pub enum ConfigureIntent {
    /// A configure is not needed (no changes to server pending state).
    NotNeeded,
    /// A configure is throttled (due to resizing too fast for example).
    Throttled,
    /// Can send the configure if it isn't throttled externally (only size changed).
    CanSend,
    /// Should send the configure regardless of external throttling (something other than size
    /// changed).
    ShouldSend,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MarkMode {
    Replace,
    Add,
    Toggle,
}

/// Tile that was just removed from the layout.
pub struct RemovedTile<W: LayoutElement> {
    tile: Tile<W>,
    /// Width of the root tiling subtree the tile was in.
    width: ColumnWidth,
    /// Whether the tile was floating.
    is_floating: bool,
}

impl<W: LayoutElement> RemovedTile<W> {
    /// Apply the sizing part of sway's cross-workspace move without replacing the node.
    ///
    /// Floating containers keep their fractions when they change workspace. Tiled containers
    /// are attached with both fractions unset, so the destination arrange derives their new
    /// share from their new siblings.
    ///
    /// sway/commands/move.c:198-239
    fn prepare_for_workspace_move(&mut self) {
        if !self.is_floating {
            self.tile.unset_node_fractions();
        }
    }
}

/// Whether to activate a newly added window.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub enum ActivateWindow {
    /// Activate unconditionally.
    Yes,
    /// Activate based on heuristics.
    #[default]
    Smart,
    /// Do not activate.
    No,
}

/// Where to put a newly added window.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub enum AddWindowTarget<'a, W: LayoutElement> {
    /// No particular preference.
    #[default]
    Auto,
    /// On this output.
    Output(&'a Output),
    /// On this workspace.
    Workspace(WorkspaceId),
    /// Next to this existing window.
    NextTo(&'a W::Id),
}

/// Type of the window hit from `window_under()`.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum HitType {
    /// The hit is within a window's input region and can be used for sending events to it.
    Input {
        /// Position of the window's buffer.
        win_pos: Point<f64, Logical>,
    },
    /// The hit can activate a window, but it is not in the input region so cannot send events.
    ///
    /// For example, this could be clicking on a tile border outside the window.
    Activate {
        /// Whether the hit was on the tab indicator.
        is_tab_indicator: bool,
    },
}

#[derive(Debug)]
enum OverviewProgress {
    Animation(Animation),
    Gesture(OverviewGesture),
    Open,
}

#[derive(Debug)]
struct OverviewGesture {
    tracker: SwipeTracker,
    /// Start point.
    start: f64,
    /// Current progress.
    value: f64,
}

/// Layer of windows to render.
#[derive(Clone, Copy)]
pub enum RenderLayer {
    Normal,
    /// Windows currently moving between workspaces.
    MovingBetweenWorkspaces,
}

impl SizingMode {
    #[must_use]
    pub fn is_normal(&self) -> bool {
        matches!(self, Self::Normal)
    }

    #[must_use]
    pub fn is_fullscreen(&self) -> bool {
        matches!(self, Self::Fullscreen)
    }
}

impl<W: LayoutElement> InteractiveMoveState<W> {
    fn moving(&self) -> Option<&InteractiveMoveData<W>> {
        match self {
            InteractiveMoveState::Moving(move_) => Some(move_),
            _ => None,
        }
    }

    fn moving_mut(&mut self) -> Option<&mut InteractiveMoveData<W>> {
        match self {
            InteractiveMoveState::Moving(move_) => Some(move_),
            _ => None,
        }
    }

    fn moving_container(&self) -> Option<&InteractiveMoveContainerData<W>> {
        match self {
            InteractiveMoveState::MovingContainer(move_) => Some(move_),
            _ => None,
        }
    }

    fn moving_window_id(&self) -> Option<&W::Id> {
        match self {
            InteractiveMoveState::Moving(move_) => Some(move_.tile.window().id()),
            InteractiveMoveState::MovingContainer(move_) => Some(&move_.window_id),
            _ => None,
        }
    }

    fn is_moving(&self) -> bool {
        matches!(
            self,
            InteractiveMoveState::Moving(_) | InteractiveMoveState::MovingContainer(_)
        )
    }
}

impl<W: LayoutElement> InteractiveMoveData<W> {
    fn tile_render_location(&self, zoom: f64) -> Point<f64, Logical> {
        let scale = Scale::from(self.output.current_scale().fractional_scale());
        let window_size = self.tile.window_size();
        let pointer_offset_within_window = Point::from((
            window_size.w * self.pointer_ratio_within_window.0,
            window_size.h * self.pointer_ratio_within_window.1,
        ));
        let pos = self.pointer_pos_within_output
            - (pointer_offset_within_window + self.tile.window_loc() - self.tile.render_offset())
                .upscale(zoom);
        // Round to physical pixels.
        pos.to_physical_precise_round(scale).to_logical(scale)
    }
}

impl ActivateWindow {
    pub fn map_smart(self, f: impl FnOnce() -> bool) -> bool {
        match self {
            ActivateWindow::Yes => true,
            ActivateWindow::Smart => f(),
            ActivateWindow::No => false,
        }
    }
}

impl HitType {
    pub fn offset_win_pos(mut self, offset: Point<f64, Logical>) -> Self {
        match &mut self {
            HitType::Input { win_pos } => *win_pos += offset,
            HitType::Activate { .. } => (),
        }
        self
    }

    pub fn hit_tile<W: LayoutElement>(
        tile: &Tile<W>,
        tile_pos: Point<f64, Logical>,
        point: Point<f64, Logical>,
    ) -> Option<(&W, Self)> {
        let pos_within_tile = point - tile_pos;
        tile.hit(pos_within_tile)
            .map(|hit| (tile.window(), hit.offset_win_pos(tile_pos)))
    }

    pub fn to_activate(self) -> Self {
        match self {
            HitType::Input { .. } => HitType::Activate {
                is_tab_indicator: false,
            },
            HitType::Activate { .. } => self,
        }
    }
}

impl Options {
    fn from_config(config: &Config) -> Self {
        Self {
            layout: config.layout.clone(),
            animations: config.animations.clone(),
            gestures: config.gestures,
            overview: config.overview,
            blur: config.blur,
            disable_resize_throttling: config.debug.disable_resize_throttling,
            deactivate_unfocused_windows: config.debug.deactivate_unfocused_windows,
        }
    }

    fn with_merged_layout(mut self, part: Option<&tiri_config::LayoutPart>) -> Self {
        if let Some(part) = part {
            self.layout.merge_with(part);
        }
        self
    }

    fn adjusted_for_scale(mut self, scale: f64) -> Self {
        self.layout.gaps = round_logical_in_physical_max1(scale, self.layout.gaps);
        self
    }
}

impl OverviewProgress {
    fn value(&self) -> f64 {
        match self {
            OverviewProgress::Animation(anim) => anim.value(),
            OverviewProgress::Gesture(gesture) => gesture.value,
            OverviewProgress::Open => 1.,
        }
    }

    fn is_animation(&self) -> bool {
        matches!(self, OverviewProgress::Animation(_))
    }
}

impl RenderLayer {
    /// Returns `true` if the render layer is [`Normal`].
    ///
    /// [`Normal`]: RenderLayer::Normal
    #[must_use]
    pub fn is_normal(&self) -> bool {
        matches!(self, Self::Normal)
    }
}

impl<W: LayoutElement> Layout<W> {
    pub fn new(clock: Clock, config: &Config) -> Self {
        Self::with_options_and_workspaces(clock, config, Options::from_config(config))
    }

    pub fn with_options(clock: Clock, options: Options) -> Self {
        let options = Rc::new(options);
        Self {
            monitor_set: MonitorSet::NoOutputs { workspaces: vec![] },
            is_active: true,
            last_active_workspace_id: HashMap::new(),
            seat_focus: SeatFocusStack::new(),
            interactive_move: None,
            dnd: None,
            scratchpad: Workspace::new_scratchpad(clock.clone(), options.clone()),
            clock,
            update_render_elements_time: Duration::ZERO,
            overview_open: false,
            overview_progress: None,
            options,
            refresh_requested: false,
        }
    }

    fn with_options_and_workspaces(clock: Clock, config: &Config, options: Options) -> Self {
        let opts = Rc::new(options);

        let workspaces = config
            .workspaces
            .iter()
            .map(|ws| {
                Workspace::new_with_config_no_outputs(Some(ws.clone()), clock.clone(), opts.clone())
            })
            .collect();

        Self {
            monitor_set: MonitorSet::NoOutputs { workspaces },
            is_active: true,
            last_active_workspace_id: HashMap::new(),
            seat_focus: SeatFocusStack::new(),
            interactive_move: None,
            dnd: None,
            scratchpad: Workspace::new_scratchpad(clock.clone(), opts.clone()),
            clock,
            update_render_elements_time: Duration::ZERO,
            overview_open: false,
            overview_progress: None,
            options: opts,
            refresh_requested: false,
        }
    }

    fn request_refresh(&mut self) {
        self.refresh_requested = true;
    }

    pub(crate) fn take_refresh_request(&mut self) -> bool {
        mem::take(&mut self.refresh_requested)
    }

    pub fn add_output(&mut self, output: Output, layout_config: Option<LayoutPart>) {
        self.monitor_set = match mem::take(&mut self.monitor_set) {
            MonitorSet::Normal {
                mut monitors,
                primary_idx,
                active_monitor_idx,
            } => {
                let primary = &mut monitors[primary_idx];

                let mut stopped_primary_ws_switch = false;

                let mut workspaces = vec![];
                for i in (0..primary.workspaces.len()).rev() {
                    if primary.workspaces[i].original_output.matches(&output) {
                        let ws = primary.workspaces.remove(i);

                        // FIXME: this can be coded in a way that the workspace switch won't be
                        // affected if the removed workspace is invisible. But this is good enough
                        // for now.
                        if primary.workspace_switch.is_some() {
                            primary.workspace_switch = None;
                            stopped_primary_ws_switch = true;
                        }

                        // The user could've closed a window while remaining on this workspace, on
                        // another monitor. However, we will add an empty workspace in the end
                        // instead.
                        if ws.has_windows_or_persistent_identity() {
                            workspaces.push(ws);
                        }

                        if i <= primary.active_workspace_idx
                            // Generally when moving the currently active workspace, we want to
                            // fall back to the workspace above, so as not to end up on the last
                            // empty workspace. However, with empty workspace above first, when
                            // moving the workspace at index 1 (first non-empty), we want to stay
                            // at index 1, so as once again not to end up on an empty workspace.
                            //
                            // This comes into play at compositor startup when having named
                            // workspaces set up across multiple monitors. Without this check, the
                            // first monitor to connect can end up with the first empty workspace
                            // focused instead of the first named workspace.
                            && !(primary.options.layout.empty_workspace_above_first
                                && primary.active_workspace_idx == 1)
                        {
                            primary.active_workspace_idx =
                                primary.active_workspace_idx.saturating_sub(1);
                        }
                    }
                }

                // If we stopped a workspace switch, then we might need to clean up workspaces.
                // Also if empty_workspace_above_first is set and there are only 2 workspaces left,
                // both will be empty and one of them needs to be removed. clean_up_workspaces
                // takes care of this.

                if stopped_primary_ws_switch
                    || (primary.options.layout.empty_workspace_above_first
                        && primary.workspaces.len() == 2)
                {
                    primary.clean_up_workspaces();
                }

                workspaces.reverse();

                let ws_id_to_activate = self.last_active_workspace_id.remove(&output.name());

                let mut monitor = Monitor::new(
                    output,
                    workspaces,
                    ws_id_to_activate,
                    self.clock.clone(),
                    self.options.clone(),
                    layout_config,
                );
                monitor.overview_open = self.overview_open;
                monitor.set_overview_progress(self.overview_progress.as_ref());
                monitors.push(monitor);

                MonitorSet::Normal {
                    monitors,
                    primary_idx,
                    active_monitor_idx,
                }
            }
            MonitorSet::NoOutputs { workspaces } => {
                let seed_initial_workspace = workspaces.is_empty();
                let ws_id_to_activate = self.last_active_workspace_id.remove(&output.name());

                let mut monitor = Monitor::new(
                    output,
                    workspaces,
                    ws_id_to_activate,
                    self.clock.clone(),
                    self.options.clone(),
                    layout_config,
                );
                if seed_initial_workspace {
                    monitor.set_initial_numeric_workspace(1, WorkspaceLifetime::Transient);
                }
                monitor.overview_open = self.overview_open;
                monitor.set_overview_progress(self.overview_progress.as_ref());

                MonitorSet::Normal {
                    monitors: vec![monitor],
                    primary_idx: 0,
                    active_monitor_idx: 0,
                }
            }
        };
        self.seat_focus_after_mutation();
    }

    pub fn remove_output(&mut self, output: &Output) {
        self.monitor_set = match mem::take(&mut self.monitor_set) {
            MonitorSet::Normal {
                mut monitors,
                mut primary_idx,
                mut active_monitor_idx,
            } => {
                let idx = monitors
                    .iter()
                    .position(|mon| &mon.output == output)
                    .expect("trying to remove non-existing output");
                let monitor = monitors.remove(idx);

                self.last_active_workspace_id.insert(
                    monitor.output_name().clone(),
                    monitor.workspaces[monitor.active_workspace_idx].id(),
                );

                let mut workspaces = monitor.into_workspaces();

                if monitors.is_empty() {
                    // Removed the last monitor.

                    for ws in &mut workspaces {
                        // Reset base options to layout ones.
                        ws.update_config(self.options.clone());
                    }

                    MonitorSet::NoOutputs { workspaces }
                } else {
                    if primary_idx >= idx {
                        // Update primary_idx to either still point at the same monitor, or at some
                        // other monitor if the primary has been removed.
                        primary_idx = primary_idx.saturating_sub(1);
                    }
                    if active_monitor_idx >= idx {
                        // Update active_monitor_idx to either still point at the same monitor, or
                        // at some other monitor if the active monitor has
                        // been removed.
                        active_monitor_idx = active_monitor_idx.saturating_sub(1);
                    }

                    let primary = &mut monitors[primary_idx];
                    primary.append_workspaces(workspaces);

                    MonitorSet::Normal {
                        monitors,
                        primary_idx,
                        active_monitor_idx,
                    }
                }
            }
            MonitorSet::NoOutputs { .. } => {
                panic!("tried to remove output when there were already none")
            }
        };
        self.seat_focus_after_mutation();
    }

    pub fn add_root_tiling_subtree_by_idx(
        &mut self,
        monitor_idx: usize,
        workspace_idx: usize,
        subtree: RootTilingSubtree<W>,
        activate: bool,
    ) {
        let MonitorSet::Normal {
            monitors,
            active_monitor_idx,
            ..
        } = &mut self.monitor_set
        else {
            panic!("add_root_tiling_subtree_by_idx requires connected outputs")
        };

        monitors[monitor_idx].add_root_tiling_subtree(workspace_idx, subtree, activate);

        if activate {
            *active_monitor_idx = monitor_idx;
        }
    }

    pub fn add_column_by_idx(
        &mut self,
        monitor_idx: usize,
        workspace_idx: usize,
        column: Column<W>,
        activate: bool,
    ) {
        self.add_root_tiling_subtree_by_idx(monitor_idx, workspace_idx, column.into(), activate);
    }

    /// Adds a new window to the layout.
    ///
    /// Returns an output that the window was added to, if there were any outputs.
    #[allow(clippy::too_many_arguments)]
    pub fn add_window(
        &mut self,
        window: W,
        target: AddWindowTarget<W>,
        width: Option<PresetSize>,
        height: Option<PresetSize>,
        is_floating: bool,
        activate: ActivateWindow,
    ) -> Option<Output> {
        let tiling_height = height.map(SizeChange::from);
        let id = window.id().clone();

        let output = match &mut self.monitor_set {
            MonitorSet::Normal {
                monitors,
                active_monitor_idx,
                ..
            } => {
                let (mon_idx, target) = match target {
                    AddWindowTarget::Auto => (*active_monitor_idx, MonitorAddWindowTarget::Auto),
                    AddWindowTarget::Output(output) => {
                        let mon_idx = monitors
                            .iter()
                            .position(|mon| mon.output == *output)
                            .unwrap();

                        (mon_idx, MonitorAddWindowTarget::Auto)
                    }
                    AddWindowTarget::Workspace(ws_id) => {
                        let mon_idx = monitors.iter().position(|mon| mon.has_ws(ws_id)).unwrap();

                        (
                            mon_idx,
                            MonitorAddWindowTarget::Workspace {
                                id: ws_id,
                                column_idx: None,
                            },
                        )
                    }
                    AddWindowTarget::NextTo(next_to) => {
                        if let Some(output) = self
                            .interactive_move
                            .as_ref()
                            .and_then(|move_| {
                                if let InteractiveMoveState::Moving(move_) = move_ {
                                    Some(move_)
                                } else {
                                    None
                                }
                            })
                            .filter(|move_| next_to == move_.tile.window().id())
                            .map(|move_| move_.output.clone())
                        {
                            // The next_to window is being interactively moved.
                            let mon_idx = monitors
                                .iter()
                                .position(|mon| mon.output == output)
                                .unwrap_or(*active_monitor_idx);

                            (mon_idx, MonitorAddWindowTarget::Auto)
                        } else {
                            let mon_idx = monitors
                                .iter()
                                .position(|mon| {
                                    mon.workspaces.iter().any(|ws| ws.has_window(next_to))
                                })
                                .unwrap();
                            (mon_idx, MonitorAddWindowTarget::NextTo(next_to))
                        }
                    }
                };
                let mon = &mut monitors[mon_idx];

                let (ws_idx, _) = mon.resolve_add_window_target(target);
                let ws = &mon.workspaces[ws_idx];
                let tiling_width = ws.resolve_tiling_width(&window, width);

                mon.add_window(window, target, activate, tiling_width, is_floating);

                if activate.map_smart(|| false) {
                    *active_monitor_idx = mon_idx;
                }

                // Set the default height for tiling windows.
                if !is_floating {
                    if let Some(change) = tiling_height {
                        let ws = mon
                            .workspaces
                            .iter_mut()
                            .find(|ws| ws.has_window(&id))
                            .unwrap();
                        ws.set_window_height(Some(&id), change);
                    }
                }

                Some(mon.output.clone())
            }
            MonitorSet::NoOutputs { workspaces } => {
                let (ws_idx, target) = match target {
                    AddWindowTarget::Auto => {
                        if workspaces.is_empty() {
                            workspaces.push(Workspace::new_no_outputs(
                                self.clock.clone(),
                                self.options.clone(),
                            ));
                        }

                        (0, WorkspaceAddWindowTarget::Auto)
                    }
                    AddWindowTarget::Output(_) => {
                        panic!("cannot target an output for a new window when none are connected")
                    }
                    AddWindowTarget::Workspace(ws_id) => {
                        let ws_idx = workspaces.iter().position(|ws| ws.id() == ws_id).unwrap();
                        (ws_idx, WorkspaceAddWindowTarget::Auto)
                    }
                    AddWindowTarget::NextTo(next_to) => {
                        if self
                            .interactive_move
                            .as_ref()
                            .and_then(|move_| {
                                if let InteractiveMoveState::Moving(move_) = move_ {
                                    Some(move_)
                                } else {
                                    None
                                }
                            })
                            .filter(|move_| next_to == move_.tile.window().id())
                            .is_some()
                        {
                            // The next_to window is being interactively moved. If there are no
                            // other windows, we may have no workspaces at all.
                            if workspaces.is_empty() {
                                workspaces.push(Workspace::new_no_outputs(
                                    self.clock.clone(),
                                    self.options.clone(),
                                ));
                            }

                            (0, WorkspaceAddWindowTarget::Auto)
                        } else {
                            let ws_idx = workspaces
                                .iter()
                                .position(|ws| ws.has_window(next_to))
                                .unwrap();
                            (ws_idx, WorkspaceAddWindowTarget::NextTo(next_to))
                        }
                    }
                };
                let ws = &mut workspaces[ws_idx];

                let tiling_width = ws.resolve_tiling_width(&window, width);

                let tile = ws.make_tile(window);
                ws.add_tile(tile, target, activate, tiling_width, is_floating, None);

                // Set the default height for tiling windows.
                if !is_floating {
                    if let Some(change) = tiling_height {
                        ws.set_window_height(Some(&id), change);
                    }
                }

                None
            }
        };
        self.seat_focus_after_mutation();
        output
    }

    pub fn remove_window(
        &mut self,
        window: &W::Id,
        transaction: Transaction,
    ) -> Option<RemovedTile<W>> {
        let removed = self.remove_window_inner(window, transaction);
        if removed.is_some() {
            self.seat_focus_after_mutation();
        }
        removed
    }

    fn remove_window_inner(
        &mut self,
        window: &W::Id,
        transaction: Transaction,
    ) -> Option<RemovedTile<W>> {
        if let Some(state) = &self.interactive_move {
            match state {
                InteractiveMoveState::Starting { window_id, .. } => {
                    if window_id == window {
                        self.interactive_move_end(window);
                    }
                }
                InteractiveMoveState::Moving(move_) => {
                    if move_.tile.window().id() == window {
                        let Some(InteractiveMoveState::Moving(move_)) =
                            self.interactive_move.take()
                        else {
                            unreachable!()
                        };

                        for mon in self.monitors_mut() {
                            mon.dnd_scroll_gesture_end();
                        }

                        return Some(RemovedTile {
                            tile: move_.tile,
                            width: move_.width,
                            is_floating: false,
                        });
                    }
                }
                InteractiveMoveState::MovingContainer(move_) => {
                    if &move_.window_id == window {
                        self.interactive_move_end(window);
                    }
                }
            }
        }

        if self.scratchpad.has_window(window) {
            let tile = self
                .scratchpad
                .take_tile_for_scratchpad(window)
                .expect("the scratchpad said it had this window");
            return Some(RemovedTile {
                width: ColumnWidth::Fixed(tile.tile_expected_or_current_size().w as i32),
                tile,
                is_floating: true,
            });
        }

        if let Some(mon) = self
            .monitors_mut()
            .find(|mon| mon.has_sticky_window(window))
        {
            let mut removed = mon.take_sticky_window(window)?;
            removed.tile.set_sticky(false);

            if mon.sticky_is_active()
                && mon.sticky_active_window_id().is_some_and(|id| id == window)
            {
                mon.clear_sticky_focus();
            }

            return Some(removed);
        }

        match &mut self.monitor_set {
            MonitorSet::Normal { monitors, .. } => {
                for mon in monitors {
                    let last_workspace_idx = mon.workspaces.len() - 1;
                    let empty_workspace_above_first =
                        mon.options.layout.empty_workspace_above_first;
                    for (idx, ws) in mon.workspaces.iter_mut().enumerate() {
                        if ws.has_window(window) {
                            let removed = ws.remove_tile(window, transaction);

                            let is_internal_placeholder = idx == last_workspace_idx
                                || (empty_workspace_above_first && idx == 0);
                            if ws.should_remove_when_empty(
                                idx == mon.active_workspace_idx,
                                is_internal_placeholder,
                            ) && mon.workspace_switch.is_none()
                            {
                                mon.workspaces.remove(idx);

                                if idx < mon.active_workspace_idx {
                                    mon.active_workspace_idx -= 1;
                                }
                            }

                            // Special case handling when empty_workspace_above_first is set and all
                            // workspaces are empty.
                            if mon.options.layout.empty_workspace_above_first
                                && mon.workspaces.len() == 2
                                && mon.workspace_switch.is_none()
                            {
                                assert!(!mon.workspaces[0].has_windows_or_persistent_identity());
                                assert!(!mon.workspaces[1].has_windows_or_persistent_identity());
                                mon.workspaces.remove(1);
                                mon.active_workspace_idx = 0;
                            }
                            return Some(removed);
                        }
                    }
                }
            }
            MonitorSet::NoOutputs { workspaces, .. } => {
                for (idx, ws) in workspaces.iter_mut().enumerate() {
                    if ws.has_window(window) {
                        let removed = ws.remove_tile(window, transaction);

                        // Clean up empty workspaces.
                        if !ws.has_windows_or_persistent_identity() {
                            workspaces.remove(idx);
                        }

                        return Some(removed);
                    }
                }
            }
        }

        None
    }

    pub fn descendants_added(&mut self, id: &W::Id) -> bool {
        for ws in self.workspaces_mut() {
            if ws.descendants_added(id) {
                return true;
            }
        }

        false
    }

    pub fn update_window(&mut self, window: &W::Id, serial: Option<Serial>) {
        if let Some(InteractiveMoveState::Moving(move_)) = &mut self.interactive_move {
            if move_.tile.window().id() == window {
                // Do this before calling update_window() so it can get up-to-date info.
                if let Some(serial) = serial {
                    move_.tile.window_mut().on_commit(serial);
                }

                move_.tile.update_window();
                return;
            }
        }

        if let Some(tile) = self
            .scratchpad
            .tiles_mut()
            .find(|tile| tile.window().id() == window)
        {
            if let Some(serial) = serial {
                tile.window_mut().on_commit(serial);
            }
            tile.update_window();
            return;
        }

        if let Some(tile) = self
            .monitors_mut()
            .flat_map(|mon| mon.sticky_tiles_mut())
            .find(|tile| tile.window().id() == window)
        {
            if let Some(serial) = serial {
                tile.window_mut().on_commit(serial);
            }
            tile.update_window();
            return;
        }

        match &mut self.monitor_set {
            MonitorSet::Normal { monitors, .. } => {
                for mon in monitors {
                    for ws in &mut mon.workspaces {
                        if ws.has_window(window) {
                            ws.update_window(window, serial);
                            return;
                        }
                    }
                }
            }
            MonitorSet::NoOutputs { workspaces, .. } => {
                for ws in workspaces {
                    if ws.has_window(window) {
                        ws.update_window(window, serial);
                        return;
                    }
                }
            }
        }
    }

    pub fn find_workspace_by_id(&self, id: WorkspaceId) -> Option<(usize, &Workspace<W>)> {
        match &self.monitor_set {
            MonitorSet::Normal { ref monitors, .. } => {
                for mon in monitors {
                    if let Some(index) = mon.idx_of_ws(id) {
                        let workspace = &mon.workspaces[index];
                        return Some((index, workspace));
                    }
                }
            }
            MonitorSet::NoOutputs { workspaces } => {
                if let Some((index, workspace)) =
                    workspaces.iter().enumerate().find(|(_, w)| w.id() == id)
                {
                    return Some((index, workspace));
                }
            }
        }

        None
    }

    pub fn find_workspace_by_name(&self, workspace_name: &str) -> Option<(usize, &Workspace<W>)> {
        match &self.monitor_set {
            MonitorSet::Normal { ref monitors, .. } => {
                for mon in monitors {
                    if let Some((index, workspace)) =
                        mon.workspaces.iter().enumerate().find(|(_, w)| {
                            w.name()
                                .is_some_and(|name| name.eq_ignore_ascii_case(workspace_name))
                        })
                    {
                        return Some((index, workspace));
                    }
                }
            }
            MonitorSet::NoOutputs { workspaces } => {
                if let Some((index, workspace)) = workspaces.iter().enumerate().find(|(_, w)| {
                    w.name()
                        .is_some_and(|name| name.eq_ignore_ascii_case(workspace_name))
                }) {
                    return Some((index, workspace));
                }
            }
        }

        None
    }

    fn ensure_workspace_by_name_impl(
        &mut self,
        workspace_name: &str,
        transient: bool,
    ) -> Option<(Option<Output>, usize)> {
        if let Some((idx, ws)) = self.find_workspace_by_name(workspace_name) {
            return Some((ws.current_output().cloned(), idx));
        }

        let name = workspace_name.to_owned();

        match &mut self.monitor_set {
            MonitorSet::Normal {
                monitors,
                active_monitor_idx,
                ..
            } => {
                let mon = &mut monitors[*active_monitor_idx];

                // Insert before the trailing internal empty workspace.
                let idx = mon.named_workspace_insert_idx();
                mon.add_workspace_at(idx);
                let lifetime = if transient {
                    WorkspaceLifetime::Transient
                } else {
                    WorkspaceLifetime::Persistent
                };
                mon.workspaces[idx].set_name(name, lifetime);

                Some((Some(mon.output().clone()), idx))
            }
            MonitorSet::NoOutputs { workspaces } => {
                let idx = ensure_no_outputs_workspace_idx(
                    workspaces,
                    self.clock.clone(),
                    self.options.clone(),
                );
                let lifetime = if transient {
                    WorkspaceLifetime::Transient
                } else {
                    WorkspaceLifetime::Persistent
                };
                workspaces[idx].set_name(name, lifetime);
                Some((None, idx))
            }
        }
    }

    pub fn find_workspace_by_number(&self, number: u32) -> Option<(usize, &Workspace<W>)> {
        match &self.monitor_set {
            MonitorSet::Normal { ref monitors, .. } => {
                for mon in monitors {
                    if let Some((index, workspace)) = mon
                        .workspaces
                        .iter()
                        .enumerate()
                        .find(|(_, w)| w.numeric_number() == Some(number))
                    {
                        return Some((index, workspace));
                    }
                }
            }
            MonitorSet::NoOutputs { workspaces } => {
                if let Some((index, workspace)) = workspaces
                    .iter()
                    .enumerate()
                    .find(|(_, w)| w.numeric_number() == Some(number))
                {
                    return Some((index, workspace));
                }
            }
        }

        None
    }

    pub fn ensure_numeric_workspace(&mut self, number: u32) -> Option<(Option<Output>, usize)> {
        if let Some((idx, ws)) = self.find_workspace_by_number(number) {
            return Some((ws.current_output().cloned(), idx));
        }

        match &mut self.monitor_set {
            MonitorSet::Normal {
                monitors,
                active_monitor_idx,
                ..
            } => {
                let mon = &mut monitors[*active_monitor_idx];
                let idx = mon.add_numeric_workspace(number, WorkspaceLifetime::Transient);
                Some((Some(mon.output().clone()), idx))
            }
            MonitorSet::NoOutputs { workspaces } => {
                let idx = ensure_no_outputs_workspace_idx(
                    workspaces,
                    self.clock.clone(),
                    self.options.clone(),
                );
                workspaces[idx].set_numeric_identity(number, WorkspaceLifetime::Transient);
                Some((None, idx))
            }
        }
    }

    pub fn ensure_workspace_by_name(
        &mut self,
        workspace_name: &str,
    ) -> Option<(Option<Output>, usize)> {
        self.ensure_workspace_by_name_impl(workspace_name, false)
    }

    pub fn ensure_workspace_by_name_transient(
        &mut self,
        workspace_name: &str,
    ) -> Option<(Option<Output>, usize)> {
        self.ensure_workspace_by_name_impl(workspace_name, true)
    }

    pub fn find_workspace_by_ref(
        &mut self,
        reference: WorkspaceReference,
    ) -> Option<&mut Workspace<W>> {
        if let WorkspaceReference::Index(index) = reference {
            let workspace_id = self.find_workspace_by_number(index).map(|(_, ws)| ws.id());
            workspace_id.and_then(|id| self.workspaces_mut().find(|ws| ws.id() == id))
        } else {
            self.workspaces_mut().find(|ws| match &reference {
                WorkspaceReference::Name(ref_name) => ws
                    .name()
                    .is_some_and(|name| name.eq_ignore_ascii_case(ref_name)),
                WorkspaceReference::Id(id) => ws.id().get() == *id,
                WorkspaceReference::Index(_) => unreachable!(),
            })
        }
    }

    pub fn unname_workspace(&mut self, workspace_name: &str) {
        self.unname_workspace_by_ref(WorkspaceReference::Name(workspace_name.into()));
    }

    pub fn unname_workspace_by_ref(&mut self, reference: WorkspaceReference) {
        let id = self.find_workspace_by_ref(reference).map(|ws| ws.id());
        if let Some(id) = id {
            self.unname_workspace_by_id(id);
        }
    }

    pub fn unname_workspace_by_id(&mut self, id: WorkspaceId) {
        let changed = match &mut self.monitor_set {
            MonitorSet::Normal { monitors, .. } => {
                monitors.iter_mut().any(|mon| mon.unname_workspace(id))
            }
            MonitorSet::NoOutputs { workspaces } => {
                let Some(idx) = workspaces.iter().position(|ws| ws.id() == id) else {
                    return;
                };
                workspaces[idx].unname();

                // Clean up empty workspaces.
                if !workspaces[idx].has_windows() {
                    workspaces.remove(idx);
                }
                true
            }
        };
        if changed {
            self.seat_focus_after_mutation();
        }
    }

    pub fn find_window_and_output(&self, wl_surface: &WlSurface) -> Option<(&W, Option<&Output>)> {
        if let Some(InteractiveMoveState::Moving(move_)) = &self.interactive_move {
            if move_.tile.window().is_wl_surface(wl_surface) {
                return Some((move_.tile.window(), Some(&move_.output)));
            }
        }

        match &self.monitor_set {
            MonitorSet::Normal { monitors, .. } => {
                for mon in monitors {
                    // Check sticky windows first.
                    if let Some(window) = mon.find_sticky_wl_surface(wl_surface) {
                        return Some((window, Some(&mon.output)));
                    }

                    for ws in &mon.workspaces {
                        if let Some(window) = ws.find_wl_surface(wl_surface) {
                            return Some((window, Some(&mon.output)));
                        }
                    }
                }
            }
            MonitorSet::NoOutputs { workspaces } => {
                for ws in workspaces {
                    if let Some(window) = ws.find_wl_surface(wl_surface) {
                        return Some((window, None));
                    }
                }
            }
        }

        if let Some(window) = self
            .scratchpad
            .tiles()
            .find(|tile| tile.window().is_wl_surface(wl_surface))
            .map(|tile| tile.window())
        {
            return Some((window, None));
        }

        None
    }

    pub fn find_window_and_output_mut(
        &mut self,
        wl_surface: &WlSurface,
    ) -> Option<(&mut W, Option<&Output>)> {
        if let Some(InteractiveMoveState::Moving(move_)) = &mut self.interactive_move {
            if move_.tile.window().is_wl_surface(wl_surface) {
                return Some((move_.tile.window_mut(), Some(&move_.output)));
            }
        }

        // Find location first with immutable borrow
        enum Location {
            Sticky(usize),
            Workspace(usize),
            NoOutput,
            Scratchpad,
            NotFound,
        }

        let location = match &self.monitor_set {
            MonitorSet::Normal { monitors, .. } => {
                let mut found = Location::NotFound;
                for (idx, mon) in monitors.iter().enumerate() {
                    if mon.find_sticky_wl_surface(wl_surface).is_some() {
                        found = Location::Sticky(idx);
                        break;
                    }
                    if mon
                        .workspaces
                        .iter()
                        .any(|ws| ws.find_wl_surface(wl_surface).is_some())
                    {
                        found = Location::Workspace(idx);
                        break;
                    }
                }
                found
            }
            MonitorSet::NoOutputs { workspaces } => {
                if workspaces
                    .iter()
                    .any(|ws| ws.find_wl_surface(wl_surface).is_some())
                {
                    Location::NoOutput
                } else {
                    Location::NotFound
                }
            }
        };

        // Check scratchpad with immutable borrow
        let location = if matches!(location, Location::NotFound) {
            if self
                .scratchpad
                .tiles()
                .any(|tile| tile.window().is_wl_surface(wl_surface))
            {
                Location::Scratchpad
            } else {
                Location::NotFound
            }
        } else {
            location
        };

        // Now do the mutable lookup based on found location
        match location {
            Location::Sticky(idx) => {
                if let MonitorSet::Normal { monitors, .. } = &mut self.monitor_set {
                    let mon = &mut monitors[idx];
                    // Access sticky_floating directly to allow Rust to see that output
                    // and sticky_floating are separate fields.
                    if let Some(tile) = mon
                        .sticky_floating
                        .tiles_mut(&mut mon.sticky_containers)
                        .find(|tile| tile.window().is_wl_surface(wl_surface))
                    {
                        return Some((tile.window_mut(), Some(&mon.output)));
                    }
                }
            }
            Location::Workspace(idx) => {
                if let MonitorSet::Normal { monitors, .. } = &mut self.monitor_set {
                    let mon = &mut monitors[idx];
                    for ws in &mut mon.workspaces {
                        if let Some(window) = ws.find_wl_surface_mut(wl_surface) {
                            return Some((window, Some(&mon.output)));
                        }
                    }
                }
            }
            Location::NoOutput => {
                if let MonitorSet::NoOutputs { workspaces } = &mut self.monitor_set {
                    for ws in workspaces {
                        if let Some(window) = ws.find_wl_surface_mut(wl_surface) {
                            return Some((window, None));
                        }
                    }
                }
            }
            Location::Scratchpad => {
                if let Some(window) = self
                    .scratchpad
                    .tiles_mut()
                    .find(|tile| tile.window().is_wl_surface(wl_surface))
                    .map(|tile| tile.window_mut())
                {
                    return Some((window, None));
                }
            }
            Location::NotFound => {}
        }

        None
    }

    /// Computes the window-geometry-relative target rect for popup unconstraining.
    ///
    /// We will try to fit popups inside this rect.
    pub fn popup_target_rect(&self, window: &W::Id) -> Rectangle<f64, Logical> {
        if let Some(InteractiveMoveState::Moving(move_)) = &self.interactive_move {
            if move_.tile.window().id() == window {
                // Follow the tiling layout logic and fit the popup horizontally within the
                // window geometry.
                let width = move_.tile.window_size().w;
                let height = output_size(&move_.output).h;
                let mut target = Rectangle::from_size(Size::from((width, height)));
                // FIXME: ideally this shouldn't include the tile render offset, but the code
                // duplication would be a bit annoying for this edge case.
                target.loc.y -= move_.tile_render_location(1.).y;
                target.loc.y -= move_.tile.window_loc().y;
                return target;
            }
        }

        self.workspaces()
            .find_map(|(_, _, ws)| ws.popup_target_rect(window))
            .unwrap()
    }

    pub fn update_output_size(&mut self, output: &Output) {
        let _span = tracy_client::span!("Layout::update_output_size");

        let Some(mon) = self.monitor_for_output_mut(output) else {
            error!("monitor missing in update_output_size()");
            return;
        };

        mon.update_output_size();
    }

    pub fn activation_view_distance(&self, window: &W::Id) -> f64 {
        if self
            .interactive_move
            .as_ref()
            .and_then(InteractiveMoveState::moving_window_id)
            .is_some_and(|id| id == window)
        {
            return 0.;
        }

        for mon in self.monitors() {
            for ws in &mon.workspaces {
                if ws.has_window(window) {
                    return ws.activation_view_distance(window);
                }
            }
        }

        0.
    }

    pub fn should_trigger_focus_follows_mouse_on(&self, window: &W::Id) -> bool {
        // During an animation, it's easy to trigger focus-follows-mouse on the previous workspace,
        // especially when clicking to switch workspace on a bar of some kind. This cancels the
        // workspace switch, which is annoying and not intended.
        //
        // This function allows focus-follows-mouse to trigger only on the animation target
        // workspace.
        if self
            .interactive_move
            .as_ref()
            .and_then(InteractiveMoveState::moving_window_id)
            .is_some_and(|id| id == window)
        {
            return true;
        }

        let MonitorSet::Normal { monitors, .. } = &self.monitor_set else {
            return true;
        };

        // Sticky windows are always visible on the active workspace.
        if monitors.iter().any(|mon| mon.has_sticky_window(window)) {
            return true;
        }

        let Some((mon, ws_idx)) = monitors.iter().find_map(|mon| {
            mon.workspaces
                .iter()
                .position(|ws| ws.has_window(window))
                .map(|ws_idx| (mon, ws_idx))
        }) else {
            return true;
        };

        // During a gesture, focus-follows-mouse does not cause any unintended workspace switches.
        if let Some(WorkspaceSwitch::Gesture(_)) = mon.workspace_switch {
            return true;
        }

        ws_idx == mon.active_workspace_idx
    }

    pub fn activate_window(&mut self, window: &W::Id) {
        if self
            .interactive_move
            .as_ref()
            .and_then(InteractiveMoveState::moving_window_id)
            .is_some_and(|id| id == window)
        {
            return;
        }

        let mut changed_focus = false;
        {
            let MonitorSet::Normal {
                monitors,
                active_monitor_idx,
                ..
            } = &mut self.monitor_set
            else {
                return;
            };

            for (monitor_idx, mon) in monitors.iter_mut().enumerate() {
                if mon.activate_sticky_window(window, true) {
                    *active_monitor_idx = monitor_idx;
                    changed_focus = true;
                    break;
                }
            }

            if !changed_focus {
                'outer: for (monitor_idx, mon) in monitors.iter_mut().enumerate() {
                    for (workspace_idx, ws) in mon.workspaces.iter_mut().enumerate() {
                        if ws.activate_window(window) {
                            mon.clear_sticky_focus();
                            *active_monitor_idx = monitor_idx;

                            // If currently in the middle of a vertical swipe between the target workspace
                            // and some other, don't switch the workspace.
                            match &mon.workspace_switch {
                                Some(WorkspaceSwitch::Gesture(gesture))
                                    if gesture.current_idx.floor() == workspace_idx as f64
                                        || gesture.current_idx.ceil() == workspace_idx as f64 => {}
                                _ => mon.switch_workspace(workspace_idx),
                            }

                            changed_focus = true;
                            break 'outer;
                        }
                    }
                }
            }
        }

        if changed_focus {
            self.seat_focus_record_active_chain();
        }
    }

    pub fn activate_window_without_raising(&mut self, window: &W::Id) {
        if self
            .interactive_move
            .as_ref()
            .and_then(InteractiveMoveState::moving_window_id)
            .is_some_and(|id| id == window)
        {
            return;
        }

        let mut changed_focus = false;
        {
            let MonitorSet::Normal {
                monitors,
                active_monitor_idx,
                ..
            } = &mut self.monitor_set
            else {
                return;
            };

            for (monitor_idx, mon) in monitors.iter_mut().enumerate() {
                if mon.activate_sticky_window(window, false) {
                    *active_monitor_idx = monitor_idx;
                    changed_focus = true;
                    break;
                }
            }

            if !changed_focus {
                'outer: for (monitor_idx, mon) in monitors.iter_mut().enumerate() {
                    for (workspace_idx, ws) in mon.workspaces.iter_mut().enumerate() {
                        if ws.activate_window_without_raising(window) {
                            mon.clear_sticky_focus();
                            *active_monitor_idx = monitor_idx;

                            // If currently in the middle of a vertical swipe between the target workspace
                            // and some other, don't switch the workspace.
                            match &mon.workspace_switch {
                                Some(WorkspaceSwitch::Gesture(gesture))
                                    if gesture.current_idx.floor() == workspace_idx as f64
                                        || gesture.current_idx.ceil() == workspace_idx as f64 => {}
                                _ => mon.switch_workspace(workspace_idx),
                            }

                            changed_focus = true;
                            break 'outer;
                        }
                    }
                }
            }
        }

        if changed_focus {
            self.seat_focus_record_active_chain();
        }
    }

    pub fn active_output(&self) -> Option<&Output> {
        let MonitorSet::Normal {
            monitors,
            active_monitor_idx,
            ..
        } = &self.monitor_set
        else {
            return None;
        };

        Some(&monitors[*active_monitor_idx].output)
    }

    pub fn active_workspace(&self) -> Option<&Workspace<W>> {
        let MonitorSet::Normal {
            monitors,
            active_monitor_idx,
            ..
        } = &self.monitor_set
        else {
            return None;
        };

        let mon = &monitors[*active_monitor_idx];
        Some(&mon.workspaces[mon.active_workspace_idx])
    }

    pub fn active_workspace_mut(&mut self) -> Option<&mut Workspace<W>> {
        let MonitorSet::Normal {
            monitors,
            active_monitor_idx,
            ..
        } = &mut self.monitor_set
        else {
            return None;
        };

        let mon = &mut monitors[*active_monitor_idx];
        Some(&mut mon.workspaces[mon.active_workspace_idx])
    }

    pub fn windows_for_output(&self, output: &Output) -> impl Iterator<Item = &W> + '_ {
        let MonitorSet::Normal { monitors, .. } = &self.monitor_set else {
            panic!("windows_for_output requires connected outputs")
        };

        let moving_window = self
            .interactive_move
            .as_ref()
            .and_then(|x| x.moving())
            .filter(|move_| move_.output == *output)
            .map(|move_| move_.tile.window())
            .into_iter();

        let mon = monitors.iter().find(|mon| &mon.output == output).unwrap();
        let mon_windows = mon.workspaces.iter().flat_map(|ws| ws.windows());
        let sticky_windows = mon.sticky_windows();

        moving_window.chain(mon_windows).chain(sticky_windows)
    }

    pub fn windows_for_output_mut(&mut self, output: &Output) -> impl Iterator<Item = &mut W> + '_ {
        let MonitorSet::Normal { monitors, .. } = &mut self.monitor_set else {
            panic!("windows_for_output_mut requires connected outputs")
        };

        let moving_window = self
            .interactive_move
            .as_mut()
            .and_then(|x| x.moving_mut())
            .filter(|move_| move_.output == *output)
            .map(|move_| move_.tile.window_mut())
            .into_iter();

        let mon = monitors
            .iter_mut()
            .find(|mon| &mon.output == output)
            .unwrap();
        let mon_windows = mon.windows_mut();

        moving_window.chain(mon_windows)
    }

    pub fn with_windows(
        &self,
        mut f: impl FnMut(&W, Option<&Output>, Option<WorkspaceId>, WindowLayout),
    ) {
        if let Some(InteractiveMoveState::Moving(move_)) = &self.interactive_move {
            // We don't fill any positions for interactively moved windows.
            let layout = move_.tile.ipc_layout_template();
            f(move_.tile.window(), Some(&move_.output), None, layout);
        }

        for tile in self.scratchpad.tiles() {
            let layout = tile.ipc_layout_template();
            f(tile.window(), None, None, layout);
        }

        match &self.monitor_set {
            MonitorSet::Normal { monitors, .. } => {
                for mon in monitors {
                    for ws in &mon.workspaces {
                        for (tile, layout) in ws.tiles_with_ipc_layouts() {
                            f(tile.window(), Some(&mon.output), Some(ws.id()), layout);
                        }
                    }

                    let active_ws_id = mon.active_workspace_ref().id();
                    for (tile, layout) in mon.sticky_tiles_with_ipc_layouts() {
                        f(tile.window(), Some(&mon.output), Some(active_ws_id), layout);
                    }
                }
            }
            MonitorSet::NoOutputs { workspaces } => {
                for ws in workspaces {
                    for (tile, layout) in ws.tiles_with_ipc_layouts() {
                        f(tile.window(), None, Some(ws.id()), layout);
                    }
                }
            }
        }
    }

    pub fn with_windows_mut(&mut self, mut f: impl FnMut(&mut W, Option<&Output>)) {
        if let Some(InteractiveMoveState::Moving(move_)) = &mut self.interactive_move {
            f(move_.tile.window_mut(), Some(&move_.output));
        }

        for tile in self.scratchpad.tiles_mut() {
            f(tile.window_mut(), None);
        }

        match &mut self.monitor_set {
            MonitorSet::Normal { monitors, .. } => {
                for mon in monitors {
                    for ws in &mut mon.workspaces {
                        for win in ws.windows_mut() {
                            f(win, Some(&mon.output));
                        }
                    }

                    let output = mon.output.clone();
                    for win in mon.sticky_windows_mut() {
                        f(win, Some(&output));
                    }
                }
            }
            MonitorSet::NoOutputs { workspaces } => {
                for ws in workspaces {
                    for win in ws.windows_mut() {
                        f(win, None);
                    }
                }
            }
        }
    }

    fn active_monitor(&mut self) -> Option<&mut Monitor<W>> {
        let MonitorSet::Normal {
            monitors,
            active_monitor_idx,
            ..
        } = &mut self.monitor_set
        else {
            return None;
        };

        Some(&mut monitors[*active_monitor_idx])
    }

    /// Run a focus command on the active workspace. Sticky focus is dropped first and the
    /// seat focus chain re-recorded afterwards, so individual commands cannot forget either.
    fn with_active_workspace_focus<R>(&mut self, f: impl FnOnce(&mut Workspace<W>) -> R) {
        self.clear_sticky_focus();
        let Some(workspace) = self.active_workspace_mut() else {
            return;
        };
        let _ = f(workspace);
        self.seat_focus_record_active_chain();
    }

    fn clear_sticky_focus(&mut self) {
        if let Some(mon) = self.active_monitor() {
            mon.clear_sticky_focus();
        }
    }

    fn clear_sticky_focus_for_output(&mut self, output: &Output) {
        if let Some(mon) = self.monitor_for_output_mut(output) {
            mon.clear_sticky_focus();
        }
    }

    pub fn active_monitor_ref(&self) -> Option<&Monitor<W>> {
        let MonitorSet::Normal {
            monitors,
            active_monitor_idx,
            ..
        } = &self.monitor_set
        else {
            return None;
        };

        Some(&monitors[*active_monitor_idx])
    }

    pub fn monitors(&self) -> impl Iterator<Item = &Monitor<W>> + '_ {
        let monitors = if let MonitorSet::Normal { monitors, .. } = &self.monitor_set {
            &monitors[..]
        } else {
            &[][..]
        };

        monitors.iter()
    }

    fn monitors_mut(&mut self) -> impl Iterator<Item = &mut Monitor<W>> + '_ {
        let monitors = if let MonitorSet::Normal { monitors, .. } = &mut self.monitor_set {
            &mut monitors[..]
        } else {
            &mut [][..]
        };

        monitors.iter_mut()
    }

    /// Hand one output's layout config to its monitor. Answers whether anything changed.
    ///
    /// This is here rather than on the monitor the caller already has because
    /// `empty-workspace-above-first` going off drops a workspace
    /// ([`Monitor::update_config`]), and the seat's history is kept on `Layout`. A workspace
    /// that dies without this seeing it stays in the history as an id nothing can resolve —
    /// the same reason [`Self::update_options`], which reaches the very same removal, prunes.
    pub fn update_output_layout_config(
        &mut self,
        output: &Output,
        layout_config: Option<tiri_config::LayoutPart>,
    ) -> bool {
        let Some(mon) = self.monitors_mut().find(|mon| mon.output() == output) else {
            return false;
        };
        if !mon.update_layout_config(layout_config) {
            return false;
        }
        self.seat_focus_after_mutation();
        true
    }

    pub fn monitor_for_output(&self, output: &Output) -> Option<&Monitor<W>> {
        self.monitors().find(|mon| &mon.output == output)
    }

    pub fn monitor_for_output_mut(&mut self, output: &Output) -> Option<&mut Monitor<W>> {
        self.monitors_mut().find(|mon| &mon.output == output)
    }

    fn find_workspace_location_by_id(&self, id: WorkspaceId) -> Option<(usize, usize)> {
        match &self.monitor_set {
            MonitorSet::Normal { monitors, .. } => {
                monitors.iter().enumerate().find_map(|(monitor_idx, mon)| {
                    mon.workspaces
                        .iter()
                        .enumerate()
                        .find(|(_, ws)| ws.id() == id)
                        .map(|(workspace_idx, _)| (monitor_idx, workspace_idx))
                })
            }
            MonitorSet::NoOutputs { .. } => None,
        }
    }

    fn seat_focus_chain_for_workspace(
        &self,
        workspace_id: WorkspaceId,
    ) -> Option<Vec<SeatFocusNode<W::Id>>> {
        self.workspaces()
            .find(|(_, _, ws)| ws.id() == workspace_id)
            .map(|_| ())?;

        Some(vec![SeatFocusNode::Workspace { workspace_id }])
    }

    fn seat_focus_record_active_chain(&mut self) {
        if !self.seat_focus.has_layout_focus() {
            return;
        }

        self.seat_focus_prune();

        let chain = match &self.monitor_set {
            MonitorSet::Normal {
                monitors,
                active_monitor_idx,
                ..
            } => {
                let mon = &monitors[*active_monitor_idx];
                let ws = mon.active_workspace_ref();
                let ws_id = ws.id();
                let output_name = mon.output.name();
                let mut chain = vec![SeatFocusNode::Workspace {
                    workspace_id: ws_id,
                }];
                if mon.sticky_is_active() {
                    if let Some(window_id) = mon.sticky_active_window_id() {
                        chain.push(SeatFocusNode::Sticky {
                            output_name,
                            window_id: window_id.clone(),
                        });
                    }
                }
                chain
            }
            MonitorSet::NoOutputs { workspaces } => {
                let Some(ws) = workspaces.first() else {
                    return;
                };
                vec![SeatFocusNode::Workspace {
                    workspace_id: ws.id(),
                }]
            }
        };

        if !chain.is_empty() {
            self.seat_focus.set_has_layout_focus(true);
            self.seat_focus.set_focus_chain(chain);
        }
    }

    fn seat_focus_record_workspace_chain(&mut self, workspace_id: WorkspaceId) {
        if !self.seat_focus.has_layout_focus() {
            return;
        }

        self.seat_focus_prune();

        let Some(chain) = self.seat_focus_chain_for_workspace(workspace_id) else {
            return;
        };

        if !chain.is_empty() {
            self.seat_focus.set_has_layout_focus(true);
            self.seat_focus.set_focus_chain(chain);
        }
    }

    fn seat_focus_workspace_targets_window(
        &self,
        workspace_id: WorkspaceId,
        window_id: &W::Id,
    ) -> bool {
        self.find_workspace_by_id(workspace_id)
            .is_some_and(|(_, workspace)| workspace.focus_targets_window(window_id))
    }

    fn seat_focus_node_valid(&self, node: &SeatFocusNode<W::Id>) -> bool {
        match node {
            SeatFocusNode::Workspace { workspace_id, .. } => {
                self.find_workspace_by_id(*workspace_id).is_some()
            }
            SeatFocusNode::Sticky {
                output_name,
                window_id,
            } => self
                .monitors()
                .any(|mon| mon.output.name() == *output_name && mon.has_sticky_window(window_id)),
        }
    }

    fn seat_focus_prune(&mut self) {
        let mut snapshot = self.seat_focus.snapshot();
        snapshot.retain(|node| self.seat_focus_node_valid(node));
        self.seat_focus.replace_from_snapshot(snapshot);
    }

    /// The layout-wide history owns scopes only; node focus is verified by each workspace.
    fn verify_seat_focus(&self) {
        assert!(
            self.seat_focus.len() <= self.seat_focus.max_len(),
            "the seat's MRU is bounded: nothing prunes it on every mutation, so the bound is \
             what stands between it and one entry per focus change for the life of the session"
        );

        // Compared by what each entry names, which is what a restore looks it up by. Window
        // ids are only `PartialEq` here, so this is the quadratic form rather than a set;
        // the stack is bounded and this runs under `debug_assertions`.
        let stack = self.seat_focus.snapshot();
        for (idx, node) in stack.iter().enumerate() {
            assert!(
                !stack[..idx].iter().any(|earlier| earlier == node),
                "a focus target must appear once: with the same one twice, \
                 \"most recently focused\" does not name anything"
            );
            assert!(
                self.seat_focus_node_valid(node),
                "layout focus history may only retain live workspace/sticky scopes: {node:?}"
            );
        }
    }

    fn seat_focus_after_mutation(&mut self) {
        // Mutations may remove a workspace even while focus is outside the layout.
        // Prune independently from recording the active chain so the history never
        // becomes a second source of truth for workspace lifetime.
        self.seat_focus_prune();
        self.seat_focus_record_active_chain();
    }

    fn seat_focus_restore_output(&mut self, output: &Output) {
        let output_name = output.name();
        let Some(candidate) = self
            .seat_focus
            .snapshot()
            .into_iter()
            .find(|node| match node {
                SeatFocusNode::Workspace { workspace_id } => self
                    .find_workspace_location_by_id(*workspace_id)
                    .is_some_and(|(monitor_idx, _)| match &self.monitor_set {
                        MonitorSet::Normal { monitors, .. } => {
                            monitors[monitor_idx].output.name() == output_name
                        }
                        MonitorSet::NoOutputs { .. } => false,
                    }),
                SeatFocusNode::Sticky {
                    output_name: name, ..
                } => *name == output_name,
            })
        else {
            return;
        };
        let candidate_workspace_location = match &candidate {
            SeatFocusNode::Workspace { workspace_id, .. } => self
                .find_workspace_location_by_id(*workspace_id)
                .map(|(monitor_idx, workspace_idx)| (*workspace_id, monitor_idx, workspace_idx)),
            SeatFocusNode::Sticky { .. } => None,
        };

        let mut target_workspace = None;
        let mut restored_sticky = false;
        if let MonitorSet::Normal {
            monitors,
            active_monitor_idx,
            ..
        } = &mut self.monitor_set
        {
            let Some(target_monitor_idx) = monitors.iter().position(|mon| mon.output == *output)
            else {
                return;
            };

            match &candidate {
                SeatFocusNode::Sticky { window_id, .. } => {
                    restored_sticky =
                        monitors[target_monitor_idx].activate_sticky_window(window_id, false);
                    if restored_sticky {
                        *active_monitor_idx = target_monitor_idx;
                    }
                }
                SeatFocusNode::Workspace { workspace_id, .. } => {
                    if let Some((candidate_workspace_id, monitor_idx, workspace_idx)) =
                        candidate_workspace_location
                    {
                        if *workspace_id == candidate_workspace_id
                            && monitor_idx == target_monitor_idx
                        {
                            *active_monitor_idx = target_monitor_idx;
                            monitors[target_monitor_idx].switch_workspace(workspace_idx);
                            target_workspace = Some(candidate_workspace_id);
                        }
                    }
                }
            }
        }

        if target_workspace.is_some() || restored_sticky {
            self.seat_focus.set_raw_focus(candidate);
        }
    }

    pub fn set_seat_layout_focus(&mut self, has_layout_focus: bool) {
        self.seat_focus.set_has_layout_focus(has_layout_focus);
        if has_layout_focus {
            self.seat_focus_record_active_chain();
        }
    }

    pub fn refresh_seat_focus(&mut self) {
        self.seat_focus_after_mutation();
    }

    pub fn monitor_for_workspace(&self, workspace_name: &str) -> Option<&Monitor<W>> {
        self.monitors().find(|monitor| {
            monitor.workspaces.iter().any(|ws| {
                ws.name()
                    .is_some_and(|name| name.eq_ignore_ascii_case(workspace_name))
            })
        })
    }

    pub fn outputs(&self) -> impl Iterator<Item = &Output> + '_ {
        self.monitors().map(|mon| &mon.output)
    }

    // Unlike the focus_* family, directional moves don't re-record the seat focus chain:
    // the focused window itself doesn't change, and the chain is re-derived on the next
    // focus command.
    pub fn move_left(&mut self) {
        let Some(workspace) = self.active_workspace_mut() else {
            return;
        };
        workspace.move_left();
    }

    pub fn move_right(&mut self) {
        let Some(workspace) = self.active_workspace_mut() else {
            return;
        };
        workspace.move_right();
    }

    pub fn move_container_left(&mut self) {
        let Some(workspace) = self.active_workspace_mut() else {
            return;
        };
        workspace.move_container_left();
    }

    pub fn move_column_left(&mut self) {
        self.move_container_left();
    }

    pub fn move_container_right(&mut self) {
        let Some(workspace) = self.active_workspace_mut() else {
            return;
        };
        workspace.move_container_right();
    }

    pub fn move_column_right(&mut self) {
        self.move_container_right();
    }

    pub fn move_root_container_to_first(&mut self) {
        let Some(workspace) = self.active_workspace_mut() else {
            return;
        };
        workspace.move_container_to_first();
    }

    pub fn move_container_to_first(&mut self) {
        self.move_root_container_to_first();
    }

    pub fn move_column_to_first(&mut self) {
        self.move_root_container_to_first();
    }

    pub fn move_root_container_to_last(&mut self) {
        let Some(workspace) = self.active_workspace_mut() else {
            return;
        };
        workspace.move_container_to_last();
    }

    pub fn move_container_to_last(&mut self) {
        self.move_root_container_to_last();
    }

    pub fn move_column_to_last(&mut self) {
        self.move_root_container_to_last();
    }

    pub fn move_container_left_or_to_output(&mut self, output: &Output) -> bool {
        if let Some(workspace) = self.active_workspace_mut() {
            if workspace.move_container_left() {
                return false;
            }
        }

        self.move_container_to_output(output, None, true);
        true
    }

    pub fn move_column_left_or_to_output(&mut self, output: &Output) -> bool {
        self.move_container_left_or_to_output(output)
    }

    pub fn move_container_right_or_to_output(&mut self, output: &Output) -> bool {
        if let Some(workspace) = self.active_workspace_mut() {
            if workspace.move_container_right() {
                return false;
            }
        }

        self.move_container_to_output(output, None, true);
        true
    }

    pub fn move_column_right_or_to_output(&mut self, output: &Output) -> bool {
        self.move_container_right_or_to_output(output)
    }

    pub fn move_root_container_to_index(&mut self, index: usize) {
        let Some(workspace) = self.active_workspace_mut() else {
            return;
        };
        workspace.move_container_to_index(index);
    }

    pub fn move_container_to_index(&mut self, index: usize) {
        self.move_root_container_to_index(index);
    }

    pub fn move_column_to_index(&mut self, index: usize) {
        self.move_root_container_to_index(index);
    }

    pub fn move_down(&mut self) {
        let Some(workspace) = self.active_workspace_mut() else {
            return;
        };
        workspace.move_down();
    }

    pub fn move_up(&mut self) {
        let Some(workspace) = self.active_workspace_mut() else {
            return;
        };
        workspace.move_up();
    }

    pub fn move_down_or_to_workspace_down(&mut self) {
        let Some(monitor) = self.active_monitor() else {
            return;
        };
        monitor.move_down_or_to_workspace_down();
    }

    pub fn move_up_or_to_workspace_up(&mut self) {
        let Some(monitor) = self.active_monitor() else {
            return;
        };
        monitor.move_up_or_to_workspace_up();
    }

    pub fn consume_or_expel_window_left(&mut self, window: Option<&W::Id>) {
        if let Some(InteractiveMoveState::Moving(move_)) = &mut self.interactive_move {
            if window.is_none() || window == Some(move_.tile.window().id()) {
                return;
            }
        }

        let workspace = if let Some(window) = window {
            if self.monitors().any(|mon| mon.has_sticky_window(window)) {
                return;
            }
            self.workspaces_mut().find(|ws| ws.has_window(window))
        } else {
            self.active_workspace_mut()
        };

        let Some(workspace) = workspace else {
            return;
        };
        workspace.consume_or_expel_window_left(window);
    }

    pub fn consume_or_expel_window_right(&mut self, window: Option<&W::Id>) {
        if let Some(InteractiveMoveState::Moving(move_)) = &mut self.interactive_move {
            if window.is_none() || window == Some(move_.tile.window().id()) {
                return;
            }
        }

        let workspace = if let Some(window) = window {
            if self.monitors().any(|mon| mon.has_sticky_window(window)) {
                return;
            }
            self.workspaces_mut().find(|ws| ws.has_window(window))
        } else {
            self.active_workspace_mut()
        };

        let Some(workspace) = workspace else {
            return;
        };
        workspace.consume_or_expel_window_right(window);
    }

    pub fn focus_left(&mut self) {
        self.with_active_workspace_focus(|ws| ws.focus_left());
    }

    pub fn focus_right(&mut self) {
        self.with_active_workspace_focus(|ws| ws.focus_right());
    }

    pub fn focus_root_container_first(&mut self) {
        self.with_active_workspace_focus(|ws| ws.focus_root_container_first());
    }

    pub fn focus_root_container_last(&mut self) {
        self.with_active_workspace_focus(|ws| ws.focus_root_container_last());
    }

    pub fn focus_root_container_right_or_first(&mut self) {
        self.with_active_workspace_focus(|ws| ws.focus_column_right_or_first());
    }

    pub fn focus_root_container_left_or_last(&mut self) {
        self.with_active_workspace_focus(|ws| ws.focus_column_left_or_last());
    }

    pub fn focus_root_container(&mut self, index: usize) {
        self.with_active_workspace_focus(|ws| ws.focus_root_container(index));
    }

    pub fn focus_column_first(&mut self) {
        self.focus_root_container_first();
    }

    pub fn focus_column_last(&mut self) {
        self.focus_root_container_last();
    }

    pub fn focus_column_right_or_first(&mut self) {
        self.focus_root_container_right_or_first();
    }

    pub fn focus_column_left_or_last(&mut self) {
        self.focus_root_container_left_or_last();
    }

    pub fn focus_column(&mut self, index: usize) {
        self.focus_root_container(index);
    }

    pub fn focus_window_up_or_output(&mut self, output: &Output) -> bool {
        self.clear_sticky_focus_for_output(output);
        if let Some(workspace) = self.active_workspace_mut() {
            if workspace.focus_up_no_wrap() {
                self.seat_focus_record_active_chain();
                return false;
            }
        }

        self.focus_output_in_direction_internal(output, Direction::Up);
        self.seat_focus_record_active_chain();
        true
    }

    pub fn focus_window_down_or_output(&mut self, output: &Output) -> bool {
        self.clear_sticky_focus_for_output(output);
        if let Some(workspace) = self.active_workspace_mut() {
            if workspace.focus_down_no_wrap() {
                self.seat_focus_record_active_chain();
                return false;
            }
        }

        self.focus_output_in_direction_internal(output, Direction::Down);
        self.seat_focus_record_active_chain();
        true
    }

    pub fn focus_container_left_or_output(&mut self, output: &Output) -> bool {
        self.clear_sticky_focus_for_output(output);
        if let Some(workspace) = self.active_workspace_mut() {
            if workspace.focus_left_no_wrap() {
                self.seat_focus_record_active_chain();
                return false;
            }
        }

        self.focus_output_in_direction_internal(output, Direction::Left);
        self.seat_focus_record_active_chain();
        true
    }

    pub fn focus_container_right_or_output(&mut self, output: &Output) -> bool {
        self.clear_sticky_focus_for_output(output);
        if let Some(workspace) = self.active_workspace_mut() {
            if workspace.focus_right_no_wrap() {
                self.seat_focus_record_active_chain();
                return false;
            }
        }

        self.focus_output_in_direction_internal(output, Direction::Right);
        self.seat_focus_record_active_chain();
        true
    }

    pub fn focus_column_left_or_output(&mut self, output: &Output) -> bool {
        self.focus_container_left_or_output(output)
    }

    pub fn focus_column_right_or_output(&mut self, output: &Output) -> bool {
        self.focus_container_right_or_output(output)
    }

    pub fn focus_window_in_column(&mut self, index: u8) {
        self.with_active_workspace_focus(|ws| ws.focus_window_in_column(index));
    }

    pub fn focus_down(&mut self) {
        self.with_active_workspace_focus(|ws| ws.focus_down());
    }

    pub fn focus_up(&mut self) {
        self.with_active_workspace_focus(|ws| ws.focus_up());
    }

    pub fn focus_down_or_left(&mut self) {
        self.with_active_workspace_focus(|ws| ws.focus_down_or_left());
    }

    pub fn focus_down_or_right(&mut self) {
        self.with_active_workspace_focus(|ws| ws.focus_down_or_right());
    }

    pub fn focus_up_or_left(&mut self) {
        self.with_active_workspace_focus(|ws| ws.focus_up_or_left());
    }

    pub fn focus_up_or_right(&mut self) {
        self.with_active_workspace_focus(|ws| ws.focus_up_or_right());
    }

    pub fn focus_window_or_workspace_down(&mut self) {
        self.clear_sticky_focus();
        let Some(monitor) = self.active_monitor() else {
            return;
        };
        monitor.focus_window_or_workspace_down();
        self.seat_focus_record_active_chain();
    }

    pub fn focus_window_or_workspace_up(&mut self) {
        self.clear_sticky_focus();
        let Some(monitor) = self.active_monitor() else {
            return;
        };
        monitor.focus_window_or_workspace_up();
        self.seat_focus_record_active_chain();
    }

    pub fn focus_window_top(&mut self) {
        self.with_active_workspace_focus(|ws| ws.focus_window_top());
    }

    pub fn focus_window_bottom(&mut self) {
        self.with_active_workspace_focus(|ws| ws.focus_window_bottom());
    }

    pub fn focus_window_down_or_top(&mut self) {
        self.with_active_workspace_focus(|ws| ws.focus_window_down_or_top());
    }

    pub fn focus_window_up_or_bottom(&mut self) {
        self.with_active_workspace_focus(|ws| ws.focus_window_up_or_bottom());
    }

    pub fn move_to_workspace_up(&mut self, focus: bool) {
        self.request_refresh();
        let Some(monitor) = self.active_monitor() else {
            return;
        };
        let activate = if focus {
            ActivateWindow::Smart
        } else {
            ActivateWindow::No
        };
        monitor.move_to_workspace_up(activate);
        self.seat_focus_after_mutation();
    }

    pub fn move_to_workspace_down(&mut self, focus: bool) {
        self.request_refresh();
        let Some(monitor) = self.active_monitor() else {
            return;
        };
        let activate = if focus {
            ActivateWindow::Smart
        } else {
            ActivateWindow::No
        };
        monitor.move_to_workspace_down(activate);
        self.seat_focus_after_mutation();
    }

    pub fn move_to_workspace(
        &mut self,
        window: Option<&W::Id>,
        idx: usize,
        activate: ActivateWindow,
    ) {
        self.request_refresh();
        if let Some(InteractiveMoveState::Moving(move_)) = &mut self.interactive_move {
            if window.is_none() || window == Some(move_.tile.window().id()) {
                return;
            }
        }

        let monitor = if let Some(window) = window {
            match &mut self.monitor_set {
                MonitorSet::Normal { monitors, .. } => {
                    let Some(monitor) = monitors.iter_mut().find(|mon| mon.has_window(window))
                    else {
                        return;
                    };
                    monitor
                }
                MonitorSet::NoOutputs { .. } => {
                    return;
                }
            }
        } else {
            let Some(monitor) = self.active_monitor() else {
                return;
            };
            monitor
        };
        monitor.move_to_workspace(window, idx, activate);
        self.seat_focus_after_mutation();
    }

    pub fn move_container_to_workspace_up(&mut self, activate: bool) {
        let Some(monitor) = self.active_monitor() else {
            return;
        };
        monitor.move_container_to_workspace_up(activate);
        self.seat_focus_after_mutation();
    }

    pub fn move_column_to_workspace_up(&mut self, activate: bool) {
        let Some(monitor) = self.active_monitor() else {
            return;
        };
        monitor.move_column_to_workspace_up(activate);
        self.seat_focus_after_mutation();
    }

    pub fn move_container_to_workspace_down(&mut self, activate: bool) {
        let Some(monitor) = self.active_monitor() else {
            return;
        };
        monitor.move_container_to_workspace_down(activate);
        self.seat_focus_after_mutation();
    }

    pub fn move_column_to_workspace_down(&mut self, activate: bool) {
        let Some(monitor) = self.active_monitor() else {
            return;
        };
        monitor.move_column_to_workspace_down(activate);
        self.seat_focus_after_mutation();
    }

    pub fn move_container_to_workspace(&mut self, idx: usize, activate: bool) {
        let Some(monitor) = self.active_monitor() else {
            return;
        };
        monitor.move_container_to_workspace(idx, activate);
        self.seat_focus_after_mutation();
    }

    pub fn move_column_to_workspace(&mut self, idx: usize, activate: bool) {
        let Some(monitor) = self.active_monitor() else {
            return;
        };
        monitor.move_column_to_workspace(idx, activate);
        self.seat_focus_after_mutation();
    }

    pub fn move_container_to_workspace_by_id(
        &mut self,
        workspace_id: WorkspaceId,
        focus: bool,
    ) -> Option<Option<Output>> {
        let (idx, mut output) = {
            let (idx, ws) = self.find_workspace_by_id(workspace_id)?;
            (idx, ws.current_output().cloned())
        };

        if let Some(active) = self.active_output() {
            if output.as_ref() == Some(active) {
                output = None;
            }
        }

        if let Some(target_output) = output.as_ref() {
            self.move_container_to_output(target_output, Some(idx), focus);
        } else {
            self.move_container_to_workspace(idx, focus);
        }

        Some(output)
    }

    pub fn move_column_to_workspace_by_id(
        &mut self,
        workspace_id: WorkspaceId,
        focus: bool,
    ) -> Option<Option<Output>> {
        let (idx, mut output) = {
            let (idx, ws) = self.find_workspace_by_id(workspace_id)?;
            (idx, ws.current_output().cloned())
        };

        if let Some(active) = self.active_output() {
            if output.as_ref() == Some(active) {
                output = None;
            }
        }

        if let Some(target_output) = output.as_ref() {
            self.move_column_to_output(target_output, Some(idx), focus);
        } else {
            self.move_column_to_workspace(idx, focus);
        }

        Some(output)
    }

    pub fn switch_workspace_up(&mut self) {
        let Some(monitor) = self.active_monitor() else {
            return;
        };
        monitor.switch_workspace_up();
        self.seat_focus_record_active_chain();
    }

    pub fn switch_workspace_down(&mut self) {
        let Some(monitor) = self.active_monitor() else {
            return;
        };
        monitor.switch_workspace_down();
        self.seat_focus_record_active_chain();
    }

    pub fn switch_workspace(&mut self, idx: usize) {
        let Some(monitor) = self.active_monitor() else {
            return;
        };
        monitor.switch_workspace(idx);
        self.seat_focus_record_active_chain();
    }

    pub fn switch_workspace_auto_back_and_forth(&mut self, idx: usize) {
        let Some(monitor) = self.active_monitor() else {
            return;
        };
        monitor.switch_workspace_auto_back_and_forth(idx);
        self.seat_focus_record_active_chain();
    }

    pub fn switch_workspace_previous(&mut self) {
        let Some(monitor) = self.active_monitor() else {
            return;
        };
        monitor.switch_workspace_previous();
        self.seat_focus_record_active_chain();
    }

    pub fn focus_workspace_by_id(
        &mut self,
        workspace_id: WorkspaceId,
        auto_back_and_forth: bool,
    ) -> Option<Option<Output>> {
        let (idx, mut output) = {
            let (idx, ws) = self.find_workspace_by_id(workspace_id)?;
            (idx, ws.current_output().cloned())
        };

        if let Some(active) = self.active_output() {
            if output.as_ref() == Some(active) {
                output = None;
            }
        }

        if let Some(target_output) = output.as_ref() {
            self.focus_output(target_output);
        }
        let monitor = self.active_monitor()?;
        if output.is_none() && auto_back_and_forth {
            monitor.switch_workspace_auto_back_and_forth(idx);
        } else {
            monitor.switch_workspace(idx);
        }

        self.seat_focus_record_active_chain();

        Some(output)
    }

    pub fn move_window_to_workspace_by_id(
        &mut self,
        window: Option<&W::Id>,
        workspace_id: WorkspaceId,
        activate: ActivateWindow,
    ) -> Option<Option<Output>> {
        self.request_refresh();
        let (idx, mut output) = {
            let (idx, ws) = self.find_workspace_by_id(workspace_id)?;
            (idx, ws.current_output().cloned())
        };

        // For the active-window action (`window == None`), keep the old fast path and move within
        // the active monitor when the target workspace is already on it.
        //
        // For explicit window-id moves, keep the target output intact so cross-output moves still
        // work even when that target output is currently active.
        if window.is_none() {
            if let Some(active) = self.active_output() {
                if output.as_ref() == Some(active) {
                    output = None;
                }
            }
        }

        if let Some(target_output) = output.as_ref() {
            self.move_to_output(window, target_output, Some(idx), activate);
        } else {
            self.move_to_workspace(window, idx, activate);
        }

        Some(output)
    }

    pub fn consume_into_container(&mut self) {
        let Some(workspace) = self.active_workspace_mut() else {
            return;
        };
        workspace.consume_into_container();
    }

    pub fn consume_into_column(&mut self) {
        self.consume_into_container();
    }

    pub fn expel_from_container(&mut self) {
        let Some(workspace) = self.active_workspace_mut() else {
            return;
        };
        workspace.expel_from_container();
    }

    pub fn expel_from_column(&mut self) {
        self.expel_from_container();
    }

    /// sway's `swap container with`, addressed by window rather than by direction.
    ///
    /// Only within the active workspace, which is where sway's own refusals leave the
    /// interesting cases anyway; a target on another workspace is left alone rather than
    /// dragged across one.
    pub fn swap_window_with(&mut self, target: &W::Id) -> bool {
        let Some(workspace) = self.active_workspace_mut() else {
            return false;
        };
        workspace.swap_window_with(target)
    }

    pub fn toggle_column_tabbed_display(&mut self) {
        let Some(workspace) = self.active_workspace_mut() else {
            return;
        };
        workspace.toggle_column_tabbed_display();
    }

    pub fn set_column_display(&mut self, display: ColumnDisplay) {
        let Some(workspace) = self.active_workspace_mut() else {
            return;
        };
        workspace.set_column_display(display);
    }

    pub fn center_window(&mut self, id: Option<&W::Id>) {
        if let Some(InteractiveMoveState::Moving(move_)) = &mut self.interactive_move {
            if id.is_none() || id == Some(move_.tile.window().id()) {
                return;
            }
        }

        let workspace = if let Some(id) = id {
            if let Some(mon) = self.monitors_mut().find(|mon| mon.has_sticky_window(id)) {
                mon.center_sticky_window(Some(id));
                return;
            }

            self.workspaces_mut().find(|ws| ws.has_window(id))
        } else {
            if let Some(mon) = self.active_monitor() {
                if mon.sticky_is_active() {
                    mon.center_sticky_window(None);
                    return;
                }
            }

            self.active_workspace_mut()
        };

        let Some(workspace) = workspace else {
            return;
        };
        workspace.center_window(id);
    }

    pub fn focus(&self) -> Option<&W> {
        self.focus_with_output().map(|(win, _out)| win)
    }

    pub fn close_window_ids_for_active_selection(&self) -> Vec<W::Id> {
        if let Some(workspace) = self.active_workspace() {
            let ids = workspace.close_window_ids_for_active_selection();
            if !ids.is_empty() {
                return ids;
            }
        }

        self.focus()
            .map(|window| vec![window.id().clone()])
            .unwrap_or_default()
    }

    pub fn active_selection_is_container(&self) -> bool {
        self.active_workspace()
            .is_some_and(Workspace::active_selection_is_container)
    }

    pub fn active_command_can_fullscreen(&self) -> bool {
        self.active_workspace()
            .is_some_and(Workspace::active_command_can_fullscreen)
    }

    pub fn focus_with_output(&self) -> Option<(&W, &Output)> {
        if let Some(InteractiveMoveState::Moving(move_)) = &self.interactive_move {
            return Some((move_.tile.window(), &move_.output));
        }

        let MonitorSet::Normal {
            monitors,
            active_monitor_idx,
            ..
        } = &self.monitor_set
        else {
            return None;
        };

        let mon = &monitors[*active_monitor_idx];
        mon.active_window().map(|win| (win, &mon.output))
    }

    pub fn interactive_moved_window_under(
        &self,
        output: &Output,
        pos_within_output: Point<f64, Logical>,
    ) -> Option<(&W, HitType)> {
        if let Some(move_) = self
            .interactive_move
            .as_ref()
            .and_then(InteractiveMoveState::moving)
        {
            if move_.output == *output {
                if self.overview_progress.is_some() {
                    let zoom = self.overview_zoom();
                    let tile_pos = move_.tile_render_location(zoom);
                    let pos_within_tile = (pos_within_output - tile_pos).downscale(zoom);
                    // During the overview animation, we cannot do input hits because we cannot
                    // really represent scaled windows properly.
                    let (win, hit) =
                        HitType::hit_tile(&move_.tile, Point::from((0., 0.)), pos_within_tile)?;
                    return Some((win, hit.to_activate()));
                } else {
                    let tile_pos = move_.tile_render_location(1.);
                    return HitType::hit_tile(&move_.tile, tile_pos, pos_within_output);
                }
            }
        }

        if let Some(move_) = self
            .interactive_move
            .as_ref()
            .and_then(InteractiveMoveState::moving_container)
        {
            if move_.output != *output {
                return None;
            }

            let (mon, (ws, ws_geo)) = self.monitors().find_map(|mon| {
                mon.workspaces_with_render_geo()
                    .find(|(ws, _)| ws.has_window(&move_.window_id))
                    .map(|rv| (mon, rv))
            })?;
            if mon.output() != output {
                return None;
            }

            let (tile, tile_offset, _visible) = ws
                .tiles_with_render_positions()
                .find(|(tile, _, _)| tile.window().id() == &move_.window_id)?;
            let zoom = mon.overview_zoom();
            let tile_pos = ws_geo.loc + tile_offset.upscale(zoom);

            if self.overview_progress.is_some() {
                let pos_within_tile = (pos_within_output - tile_pos).downscale(zoom);
                let (win, hit) = HitType::hit_tile(tile, Point::from((0., 0.)), pos_within_tile)?;
                return Some((win, hit.to_activate()));
            }

            return HitType::hit_tile(tile, tile_pos, pos_within_output);
        }

        None
    }

    /// Returns the window under the cursor and the hit type.
    pub fn window_under(
        &self,
        output: &Output,
        pos_within_output: Point<f64, Logical>,
    ) -> Option<(&W, HitType)> {
        let mon = self.monitor_for_output(output)?;
        mon.window_under(pos_within_output)
    }

    pub fn resize_edges_under(
        &mut self,
        output: &Output,
        pos_within_output: Point<f64, Logical>,
    ) -> Option<ResizeEdge> {
        let mon = self.monitor_for_output_mut(output)?;
        mon.resize_edges_under(pos_within_output)
    }

    pub fn resize_hit_under(
        &mut self,
        output: &Output,
        pos_within_output: Point<f64, Logical>,
    ) -> Option<ResizeHit<W::Id>> {
        let mon = self.monitor_for_output_mut(output)?;
        mon.resize_hit_under(pos_within_output)
    }

    pub fn workspace_under(
        &self,
        extended_bounds: bool,
        output: &Output,
        pos_within_output: Point<f64, Logical>,
    ) -> Option<&Workspace<W>> {
        if self
            .interactive_moved_window_under(output, pos_within_output)
            .is_some()
        {
            return None;
        }

        let mon = self.monitor_for_output(output)?;
        if extended_bounds {
            mon.workspace_under(pos_within_output).map(|(ws, _)| ws)
        } else {
            mon.workspace_under_narrow(pos_within_output)
        }
    }

    pub fn overview_zoom(&self) -> f64 {
        let progress = self.overview_progress.as_ref().map(|p| p.value());
        compute_overview_zoom(&self.options, progress)
    }

    /// True while any workspace is still waiting on a configure it sent.
    pub(crate) fn has_pending_layouts(&self) -> bool {
        self.workspaces().any(|(_, _, ws)| ws.has_pending_layouts())
    }

    /// Assert every structural invariant the layout is supposed to hold.
    ///
    /// Panics on the first violation, by design: the interesting output is the assertion and
    /// the state that reached it, and a compositor that carries a broken tree forward turns
    /// one bug into a session of unexplainable ones. The test suite runs this after every op;
    /// `debug { verify-layout-invariants; }` runs it in a live session, where the classes of
    /// bug that only a real client can produce actually live.
    pub(crate) fn verify_invariants(&self) {
        use std::collections::HashSet;

        use approx::assert_abs_diff_eq;

        self.verify_seat_focus();

        let zoom = self.overview_zoom();

        let mut move_win_id = None;
        if let Some(state) = &self.interactive_move {
            match state {
                InteractiveMoveState::Starting {
                    window_id,
                    pointer_delta: _,
                    pointer_ratio_within_window: _,
                } => {
                    assert!(
                        self.has_window(window_id),
                        "interactive move must be on an existing window"
                    );
                    move_win_id = Some(window_id.clone());
                }
                InteractiveMoveState::Moving(move_) => {
                    assert_eq!(self.clock, move_.tile.clock);
                    assert!(move_.tile.window().pending_sizing_mode().is_normal());

                    move_.tile.verify_invariants();

                    let scale = move_.output.current_scale().fractional_scale();
                    let options = Options::clone(&self.options)
                        .with_merged_layout(move_.output_config.as_ref())
                        .with_merged_layout(move_.workspace_config.as_ref().map(|(_, c)| c))
                        .adjusted_for_scale(scale);
                    assert_eq!(
                        &*move_.tile.options, &options,
                        "interactive moved tile options must be \
                         base options adjusted for output scale"
                    );

                    let tile_pos = move_.tile_render_location(zoom);
                    let rounded_pos = tile_pos.to_physical_precise_round(scale).to_logical(scale);

                    // Tile position must be rounded to physical pixels.
                    assert_abs_diff_eq!(tile_pos.x, rounded_pos.x, epsilon = 1e-5);
                    assert_abs_diff_eq!(tile_pos.y, rounded_pos.y, epsilon = 1e-5);

                    if let Some(alpha) = &move_.tile.alpha_animation {
                        if move_.is_floating {
                            assert_eq!(
                                alpha.anim.to(),
                                1.,
                                "interactively moved floating tile can animate alpha only to 1"
                            );

                            assert!(
                                !alpha.hold_after_done,
                                "interactively moved floating tile \
                                 cannot have held alpha animation"
                            );
                        } else {
                            assert_ne!(
                                alpha.anim.to(),
                                1.,
                                "interactively moved tiling tile must animate alpha to not 1"
                            );

                            assert!(
                                alpha.hold_after_done,
                                "interactively moved tiling tile \
                                 must have held alpha animation"
                            );
                        }
                    }
                }
                InteractiveMoveState::MovingContainer(move_) => {
                    assert!(
                        self.has_window(&move_.window_id),
                        "interactive move container must be on an existing window"
                    );
                    move_win_id = Some(move_.window_id.clone());
                }
            }
        }

        let mut seen_workspace_id = HashSet::new();
        let mut seen_workspace_name = Vec::<String>::new();

        let (monitors, &primary_idx, &active_monitor_idx) = match &self.monitor_set {
            MonitorSet::Normal {
                monitors,
                primary_idx,
                active_monitor_idx,
            } => (monitors, primary_idx, active_monitor_idx),
            MonitorSet::NoOutputs { workspaces } => {
                for workspace in workspaces {
                    assert!(
                        workspace.has_windows_or_persistent_identity(),
                        "with no outputs there cannot be empty unnamed workspaces"
                    );

                    assert_eq!(self.clock, workspace.clock);

                    assert_eq!(
                        workspace.base_options, self.options,
                        "workspace base options must be synchronized with layout"
                    );

                    assert!(
                        seen_workspace_id.insert(workspace.id()),
                        "workspace id must be unique"
                    );

                    if let Some(name) = workspace.name() {
                        assert!(
                            !seen_workspace_name
                                .iter()
                                .any(|n| n.eq_ignore_ascii_case(name)),
                            "workspace name must be unique"
                        );
                        seen_workspace_name.push(name.clone());
                    }

                    workspace.verify_invariants(move_win_id.as_ref());
                }

                return;
            }
        };

        assert!(primary_idx < monitors.len());
        assert!(active_monitor_idx < monitors.len());

        let mut saw_horizontal_view_gesture = false;

        for (idx, monitor) in monitors.iter().enumerate() {
            assert_eq!(self.clock, monitor.clock);
            assert_eq!(
                monitor.base_options, self.options,
                "monitor base options must be synchronized with layout"
            );

            assert_eq!(self.overview_open, monitor.overview_open);
            assert_eq!(
                self.overview_progress.as_ref().map(|p| p.value()),
                monitor.overview_progress_value()
            );

            monitor.verify_invariants();

            if idx == primary_idx {
                for ws in &monitor.workspaces {
                    if ws.original_output.matches(&monitor.output) {
                        // This is the primary monitor's own workspace.
                        continue;
                    }

                    let own_monitor_exists = monitors
                        .iter()
                        .any(|m| ws.original_output.matches(&m.output));
                    assert!(
                        !own_monitor_exists,
                        "primary monitor cannot have workspaces for which their own monitor exists"
                    );
                }
            } else {
                assert!(
                    monitor
                        .workspaces
                        .iter()
                        .any(|workspace| workspace.original_output.matches(&monitor.output)),
                    "secondary monitor must not have any non-own workspaces"
                );
            }

            // FIXME: verify that primary doesn't have any workspaces for which their own monitor
            // exists.

            for workspace in &monitor.workspaces {
                assert!(
                    seen_workspace_id.insert(workspace.id()),
                    "workspace id must be unique"
                );

                if let Some(name) = workspace.name() {
                    assert!(
                        !seen_workspace_name
                            .iter()
                            .any(|n| n.eq_ignore_ascii_case(name)),
                        "workspace name must be unique"
                    );
                    seen_workspace_name.push(name.clone());
                }

                workspace.verify_invariants(move_win_id.as_ref());

                let has_horizontal_view_gesture = false;
                if self.dnd.is_some() || self.interactive_move.is_some() {
                    // We'd like to check that all workspaces have the gesture here, furthermore we
                    // want to check that they have the gesture only if the interactive move
                    // targets the tiling layout. However, we cannot do that because we start
                    // and stop the gesture lazily. Otherwise the gesture code would pollute a lot
                    // of places like adding new workspaces, implicitly moving windows between
                    // floating and tiling on fullscreen, etc.
                    //
                    // assert!(
                    //     has_horizontal_view_gesture,
                    //     "during an interactive move in the tiling layout, \
                    //      all workspaces should be in a horizontal view gesture"
                    // );
                } else if saw_horizontal_view_gesture {
                    assert!(
                        !has_horizontal_view_gesture,
                        "only one workspace can have an ongoing horizontal view gesture"
                    );
                }
                saw_horizontal_view_gesture = has_horizontal_view_gesture;
            }
        }
    }

    pub fn advance_animations(&mut self) {
        let _span = tracy_client::span!("Layout::advance_animations");

        let mut dnd_scroll = None;
        let mut is_dnd = false;
        if let Some(dnd) = &self.dnd {
            dnd_scroll = Some((dnd.output.clone(), dnd.pointer_pos_within_output, true));
            is_dnd = true;
        }

        if let Some(InteractiveMoveState::Moving(move_)) = &mut self.interactive_move {
            move_.tile.advance_animations();

            if dnd_scroll.is_none() {
                dnd_scroll = Some((
                    move_.output.clone(),
                    move_.pointer_pos_within_output,
                    !move_.is_floating,
                ));
            }
        }

        let is_overview_open = self.overview_open;

        // Scroll the view if needed.
        if let Some((output, pos_within_output, _is_scrolling)) = dnd_scroll {
            if let Some(mon) = self.monitor_for_output_mut(&output) {
                let mut scrolled = false;

                let zoom = mon.overview_zoom();
                scrolled |= mon.dnd_scroll_gesture_scroll(pos_within_output, 1. / zoom);

                if scrolled {
                    // Don't trigger DnD hold while scrolling.
                    if let Some(dnd) = &mut self.dnd {
                        dnd.hold = None;
                    }
                } else if is_dnd {
                    let target = mon
                        .window_under(pos_within_output)
                        .map(|(win, _)| DndHoldTarget::Window(win.id().clone()))
                        .or_else(|| {
                            mon.workspace_under_narrow(pos_within_output)
                                .map(|ws| DndHoldTarget::Workspace(ws.id()))
                        });

                    let dnd = self.dnd.as_mut().unwrap();
                    if let Some(target) = target {
                        let now = self.clock.now_unadjusted();
                        let start_time = if let Some(hold) = &mut dnd.hold {
                            if hold.target != target {
                                hold.start_time = now;
                            }
                            hold.target = target;
                            hold.start_time
                        } else {
                            let hold = dnd.hold.insert(DndHold {
                                start_time: now,
                                target,
                            });
                            hold.start_time
                        };

                        // Delay copied from gnome-shell.
                        let delay = Duration::from_millis(750);
                        if delay <= now.saturating_sub(start_time) {
                            let hold = dnd.hold.take().unwrap();

                            // Synchronize workspace switch to overview close to get a monotonic
                            // animation.
                            let config = is_overview_open
                                .then_some(self.options.animations.overview_open_close.0);

                            if let Some(mon) = self.monitor_for_output_mut(&output) {
                                let ws_idx = match hold.target {
                                    DndHoldTarget::Window(id) => mon
                                        .workspaces
                                        .iter_mut()
                                        .position(|ws| ws.activate_window(&id)),
                                    DndHoldTarget::Workspace(id) => {
                                        mon.workspaces.iter().position(|ws| ws.id() == id)
                                    }
                                };

                                if let Some(ws_idx) = ws_idx {
                                    mon.dnd_scroll_gesture_end();
                                    mon.activate_workspace_with_anim_config(ws_idx, config);

                                    self.focus_output(&output);

                                    if is_overview_open {
                                        self.close_overview();
                                    }
                                } else {
                                    error!("DnD hold target disappeared before activation");
                                }
                            } else {
                                error!("DnD hold output disappeared before activation");
                            }
                        }
                    } else {
                        // No target, reset the hold timer.
                        dnd.hold = None;
                    }
                }
            }
        }

        if let Some(OverviewProgress::Animation(anim)) = &mut self.overview_progress {
            if anim.is_done() {
                if self.overview_open {
                    self.overview_progress = Some(OverviewProgress::Open);
                } else {
                    self.overview_progress = None;
                }
            }
        }

        match &mut self.monitor_set {
            MonitorSet::Normal { monitors, .. } => {
                for mon in monitors {
                    mon.set_overview_progress(self.overview_progress.as_ref());
                    mon.advance_animations();
                }
            }
            MonitorSet::NoOutputs { workspaces, .. } => {
                for ws in workspaces {
                    ws.advance_animations();
                }
            }
        }

        self.scratchpad.advance_animations();
        // A completed workspace-switch animation may have removed transient empty
        // workspaces. Keep the layout-wide scope history live at the same mutation boundary.
        self.seat_focus_prune();
    }

    pub fn are_animations_ongoing(&self, output: Option<&Output>) -> bool {
        // Keep advancing animations if we might need to scroll the view.
        if let Some(dnd) = &self.dnd {
            if output.is_none_or(|output| *output == dnd.output) {
                return true;
            }
        }

        if let Some(InteractiveMoveState::Moving(move_)) = &self.interactive_move {
            if output.is_none_or(|output| *output == move_.output) {
                if move_.tile.are_animations_ongoing() {
                    return true;
                }

                // Keep advancing animations if we might need to scroll the view.
                if !move_.is_floating || self.overview_open {
                    return true;
                }
            }
        }

        if self
            .overview_progress
            .as_ref()
            .is_some_and(|p| p.is_animation())
        {
            return true;
        }

        for mon in self.monitors() {
            if output.is_some_and(|output| mon.output != *output) {
                continue;
            }

            if mon.are_animations_ongoing() {
                return true;
            }
        }

        false
    }

    pub fn update_render_elements(&mut self, output: Option<&Output>) {
        let _span = tracy_client::span!("Layout::update_render_elements");

        self.update_render_elements_time = self.clock.now();

        let zoom = self.overview_zoom();
        if let Some(InteractiveMoveState::Moving(move_)) = &mut self.interactive_move {
            if output.is_none_or(|output| move_.output == *output) {
                let pos_within_output = move_.tile_render_location(zoom);

                // We're not on any specific workspace so we can't compute a "workspace view" rect.
                // Let's instead compute a rect relative to the output.
                //
                // FIXME: we could make the colors match up better in the overview by figuring out
                // where a centered workspace would currently be, and computing the view rect
                // against that. Since most of the time the dragged window will be on a centered
                // workspace.
                let view_rect =
                    Rectangle::new(pos_within_output.upscale(-1.), output_size(&move_.output))
                        .downscale(zoom);
                move_.tile.update_render_elements(
                    true,
                    true,
                    true,
                    crate::layout::focus_ring::FocusRingEdges::all(),
                    None,
                    view_rect,
                );
            }
        }

        self.update_insert_hint(output);

        let MonitorSet::Normal {
            monitors,
            active_monitor_idx,
            ..
        } = &mut self.monitor_set
        else {
            if output.is_some() {
                error!("update_render_elements called with no monitors but Some output");
            }
            return;
        };

        for (idx, mon) in monitors.iter_mut().enumerate() {
            if output.is_none_or(|output| mon.output == *output) {
                let is_active = self.is_active
                    && idx == *active_monitor_idx
                    && !self
                        .interactive_move
                        .as_ref()
                        .is_some_and(InteractiveMoveState::is_moving);
                mon.set_overview_progress(self.overview_progress.as_ref());
                mon.update_render_elements(is_active);
            }
        }

        // Never active: nothing in the scratchpad is on screen. It still needs the pass, which
        // is where a branch's arrange is committed once its windows have answered.
        self.scratchpad
            .update_render_elements(false, RenderLayer::Normal);
    }

    pub fn update_shaders(&mut self) {
        if let Some(InteractiveMoveState::Moving(move_)) = &mut self.interactive_move {
            move_.tile.update_shaders();
        }

        match &mut self.monitor_set {
            MonitorSet::Normal { monitors, .. } => {
                for mon in monitors {
                    mon.update_shaders();
                }
            }
            MonitorSet::NoOutputs { workspaces, .. } => {
                for ws in workspaces {
                    ws.update_shaders();
                }
            }
        }
    }

    fn update_insert_hint(&mut self, output: Option<&Output>) {
        let _span = tracy_client::span!("Layout::update_insert_hint");

        for mon in self.monitors_mut() {
            mon.insert_hint = None;
        }

        if !matches!(self.interactive_move, Some(InteractiveMoveState::Moving(_))) {
            return;
        }
        let Some(InteractiveMoveState::Moving(move_)) = self.interactive_move.take() else {
            unreachable!()
        };
        if output.is_some_and(|out| &move_.output != out) {
            self.interactive_move = Some(InteractiveMoveState::Moving(move_));
            return;
        }

        let _span = tracy_client::span!("Layout::update_insert_hint::update");

        if let Some(mon) = self.monitor_for_output_mut(&move_.output) {
            let zoom = mon.overview_zoom();
            let (insert_ws, geo) = mon.insert_position(move_.pointer_pos_within_output);
            match insert_ws {
                InsertWorkspace::Existing(ws_id) => {
                    let idx = mon.idx_of_ws(ws_id).unwrap();
                    let ws = &mut mon.workspaces[idx];
                    let pos_within_workspace =
                        (move_.pointer_pos_within_output - geo.loc).downscale(zoom);
                    let position = if move_.is_floating {
                        InsertPosition::Floating
                    } else {
                        ws.tiling_insert_position(pos_within_workspace)
                    };

                    let border_width = move_.tile.effective_border_width().unwrap_or(0.);
                    let corner_radius = move_
                        .tile
                        .window()
                        .geometry_corner_radius()
                        .expanded_by(border_width as f32);
                    mon.insert_hint = Some(InsertHint {
                        workspace: insert_ws,
                        position,
                        corner_radius,
                    });
                }
                InsertWorkspace::NewAt(_) => {
                    let position = if move_.is_floating {
                        InsertPosition::Floating
                    } else {
                        InsertPosition::NewColumn(0)
                    };
                    mon.insert_hint = Some(InsertHint {
                        workspace: insert_ws,
                        position,
                        corner_radius: CornerRadius::default(),
                    });
                }
            }
        }

        self.interactive_move = Some(InteractiveMoveState::Moving(move_));
    }

    pub fn ensure_named_workspace(&mut self, ws_config: &WorkspaceConfig) {
        if self.find_workspace_by_name(&ws_config.name.0).is_some() {
            return;
        }

        let clock = self.clock.clone();
        let options = self.options.clone();

        match &mut self.monitor_set {
            MonitorSet::Normal {
                monitors,
                primary_idx,
                active_monitor_idx,
            } => {
                let mon_idx = ws_config
                    .open_on_output
                    .as_deref()
                    .map(|name| {
                        monitors
                            .iter_mut()
                            .position(|monitor| output_matches_name(&monitor.output, name))
                            .unwrap_or(*primary_idx)
                    })
                    .unwrap_or(*active_monitor_idx);
                let mon = &mut monitors[mon_idx];

                let ws = Workspace::new_with_config(
                    mon.output.clone(),
                    Some(ws_config.clone()),
                    clock,
                    options,
                );
                mon.insert_workspace(ws, mon.named_workspace_insert_idx(), false);
            }
            MonitorSet::NoOutputs { workspaces } => {
                let ws =
                    Workspace::new_with_config_no_outputs(Some(ws_config.clone()), clock, options);
                workspaces.push(ws);
            }
        }
        // Monitor::insert_workspace() also normalizes the transient empty workspaces around
        // the insertion.  That can retire a workspace other than the one just added, so the
        // layout-wide focus stack must cross the same mutation boundary as every other
        // workspace-lifetime change.
        self.seat_focus_after_mutation();
    }

    pub fn update_config(&mut self, config: &Config) {
        // Update workspace-specific config for all named workspaces.
        for ws in self.workspaces_mut() {
            let Some(name) = ws.name() else { continue };
            if let Some(config) = config.workspaces.iter().find(|w| &w.name.0 == name) {
                ws.update_layout_config(config.layout.clone().map(|x| x.0));
            }
        }

        self.update_options(Options::from_config(config));
    }

    fn update_options(&mut self, options: Options) {
        let options = Rc::new(options);

        if let Some(InteractiveMoveState::Moving(move_)) = &mut self.interactive_move {
            let view_size = output_size(&move_.output);
            let scale = move_.output.current_scale().fractional_scale();
            let options = Options::clone(&options)
                .with_merged_layout(move_.output_config.as_ref())
                .with_merged_layout(move_.workspace_config.as_ref().map(|(_, c)| c))
                .adjusted_for_scale(scale);
            move_.tile.update_config(view_size, scale, Rc::new(options));
        }

        match &mut self.monitor_set {
            MonitorSet::Normal { monitors, .. } => {
                for mon in monitors {
                    mon.update_config(options.clone());
                }
            }
            MonitorSet::NoOutputs { workspaces } => {
                for ws in workspaces {
                    ws.update_config(options.clone());
                }
            }
        }

        self.options = options;
        self.seat_focus_after_mutation();
    }

    pub fn toggle_width(&mut self, forwards: bool) {
        let Some(workspace) = self.active_workspace_mut() else {
            return;
        };
        workspace.toggle_width(forwards);
    }

    pub fn toggle_window_width(&mut self, window: Option<&W::Id>, forwards: bool) {
        if let Some(InteractiveMoveState::Moving(move_)) = &mut self.interactive_move {
            if window.is_none() || window == Some(move_.tile.window().id()) {
                return;
            }
        }

        let workspace = if let Some(window) = window {
            self.workspaces_mut().find(|ws| ws.has_window(window))
        } else {
            self.active_workspace_mut()
        };

        let Some(workspace) = workspace else {
            return;
        };
        workspace.toggle_window_width(window, forwards);
    }

    pub fn toggle_window_height(&mut self, window: Option<&W::Id>, forwards: bool) {
        if let Some(InteractiveMoveState::Moving(move_)) = &mut self.interactive_move {
            if window.is_none() || window == Some(move_.tile.window().id()) {
                return;
            }
        }

        let workspace = if let Some(window) = window {
            self.workspaces_mut().find(|ws| ws.has_window(window))
        } else {
            self.active_workspace_mut()
        };

        let Some(workspace) = workspace else {
            return;
        };
        workspace.toggle_window_height(window, forwards);
    }

    pub fn toggle_full_width(&mut self) {
        let Some(workspace) = self.active_workspace_mut() else {
            return;
        };
        workspace.toggle_full_width();
    }

    pub fn focus_along_parent(&mut self, forward: bool, descend: bool) {
        self.clear_sticky_focus();
        if let Some(workspace) = self.active_workspace_mut() {
            workspace.focus_along_parent(forward, descend);
            self.seat_focus_record_active_chain();
        }
    }

    pub fn focus_parent(&mut self) {
        self.clear_sticky_focus();
        if let Some(workspace) = self.active_workspace_mut() {
            workspace.focus_parent();
            self.seat_focus_record_active_chain();
        }
    }

    pub fn focus_child(&mut self) {
        self.clear_sticky_focus();
        if let Some(workspace) = self.active_workspace_mut() {
            workspace.focus_child();
            self.seat_focus_record_active_chain();
        }
    }

    pub fn split_horizontal(&mut self) {
        if let Some(workspace) = self.active_workspace_mut() {
            workspace.split_horizontal();
        }
    }

    pub fn split_vertical(&mut self) {
        if let Some(workspace) = self.active_workspace_mut() {
            workspace.split_vertical();
        }
    }

    pub fn split_none(&mut self) {
        if let Some(workspace) = self.active_workspace_mut() {
            workspace.split_none();
        }
    }

    pub fn split_toggle(&mut self) {
        if let Some(workspace) = self.active_workspace_mut() {
            workspace.split_toggle();
        }
    }

    /// Turn autotiling on or off for every workspace at once.
    ///
    /// The mode lives in [`Options`] rather than in a field of its own so that the tree reads
    /// it where it already reads gaps, and so that a workspace which overrides its layout
    /// config keeps overriding this too. The cost is that it travels the same path a config
    /// reload does, which also means a reload puts the config's value back — the file stays
    /// the authority on what the mode is when the session is told to re-read it.
    pub fn toggle_autotile(&mut self) {
        let mut options = Options::clone(&self.options);
        options.layout.autotile = !options.layout.autotile;
        self.update_options(options);
    }

    /// Whether autotiling is currently on.
    pub fn is_autotile(&self) -> bool {
        self.options.layout.autotile
    }

    pub fn set_layout_mode(&mut self, layout: ContainerLayout) {
        if let Some(workspace) = self.active_workspace_mut() {
            workspace.set_layout_mode(layout);
        }
    }

    pub fn toggle_split_layout(&mut self) {
        if let Some(workspace) = self.active_workspace_mut() {
            workspace.toggle_split_layout();
        }
    }

    pub fn toggle_layout_all(&mut self) {
        if let Some(workspace) = self.active_workspace_mut() {
            workspace.toggle_layout_all();
        }
    }

    pub fn set_default_layout(&mut self) {
        if let Some(workspace) = self.active_workspace_mut() {
            workspace.set_default_layout();
        }
    }

    pub(crate) fn toggle_layout_cycle(&mut self, cycle: &[LayoutCycleEntry]) {
        if let Some(workspace) = self.active_workspace_mut() {
            workspace.toggle_layout_cycle(cycle);
        }
    }

    pub fn set_column_width(&mut self, change: SizeChange) {
        let Some(workspace) = self.active_workspace_mut() else {
            return;
        };
        workspace.set_column_width(change);
    }

    pub fn set_window_width(&mut self, window: Option<&W::Id>, change: SizeChange) {
        self.resize_window(
            window,
            ResizeRequest::Axis {
                axis: ResizeAxis::Horizontal,
                change,
            },
        );
    }

    pub fn set_window_height(&mut self, window: Option<&W::Id>, change: SizeChange) {
        self.resize_window(
            window,
            ResizeRequest::Axis {
                axis: ResizeAxis::Vertical,
                change,
            },
        );
    }

    pub fn resize_window(&mut self, window: Option<&W::Id>, request: ResizeRequest) {
        self.request_refresh();
        if let Some(InteractiveMoveState::Moving(move_)) = &mut self.interactive_move {
            if window.is_none() || window == Some(move_.tile.window().id()) {
                return;
            }
        }

        let workspace = if let Some(window) = window {
            self.workspaces_mut().find(|ws| ws.has_window(window))
        } else {
            self.active_workspace_mut()
        };

        let Some(workspace) = workspace else {
            return;
        };
        workspace.resize_window(window, request);
    }

    pub fn reset_window_height(&mut self, window: Option<&W::Id>) {
        self.request_refresh();
        if let Some(InteractiveMoveState::Moving(move_)) = &mut self.interactive_move {
            if window.is_none() || window == Some(move_.tile.window().id()) {
                return;
            }
        }

        let workspace = if let Some(window) = window {
            self.workspaces_mut().find(|ws| ws.has_window(window))
        } else {
            self.active_workspace_mut()
        };

        let Some(workspace) = workspace else {
            return;
        };
        workspace.reset_window_height(window);
    }

    pub fn expand_column_to_available_width(&mut self) {
        let Some(workspace) = self.active_workspace_mut() else {
            return;
        };
        workspace.expand_column_to_available_width();
    }

    pub fn toggle_window_floating(&mut self, window: Option<&W::Id>) {
        self.request_refresh();
        if let Some(InteractiveMoveState::Moving(move_)) = &mut self.interactive_move {
            if window.is_none() || window == Some(move_.tile.window().id()) {
                move_.is_floating = !move_.is_floating;

                // When going to floating, restore the floating window size.
                if move_.is_floating {
                    let floating_size = move_.tile.floating_window_size;
                    let win = move_.tile.window_mut();
                    let mut size =
                        floating_size.unwrap_or_else(|| win.expected_size().unwrap_or_default());

                    // Apply min/max size window rules. If requesting a concrete size, apply
                    // completely; if requesting (0, 0), apply only when min/max results in a fixed
                    // size.
                    let min_size = win.min_size();
                    let max_size = win.max_size();
                    size.w = ensure_min_max_size_maybe_zero(size.w, min_size.w, max_size.w);
                    size.h = ensure_min_max_size_maybe_zero(size.h, min_size.h, max_size.h);

                    win.request_size_once(size, true);

                    // Animate the tile back to opaque.
                    move_.tile.animate_alpha(
                        INTERACTIVE_MOVE_ALPHA,
                        1.,
                        self.options.animations.window_movement.0,
                    );
                } else {
                    // Animate the tile back to semitransparent.
                    move_.tile.animate_alpha(
                        1.,
                        INTERACTIVE_MOVE_ALPHA,
                        self.options.animations.window_movement.0,
                    );
                    move_.tile.hold_alpha_animation_after_done();
                }

                return;
            }
        }

        let target_workspace_id = window.and_then(|id| {
            self.workspaces()
                .find(|(_, _, ws)| ws.has_window(id))
                .map(|(_, _, ws)| ws.id())
        });
        let target_window_was_workspace_focus = match (target_workspace_id, window) {
            (Some(workspace_id), Some(window_id)) => {
                self.seat_focus_workspace_targets_window(workspace_id, window_id)
            }
            _ => false,
        };
        let workspace = if let Some(window) = window {
            self.workspaces_mut().find(|ws| ws.has_window(window))
        } else {
            self.active_workspace_mut()
        };

        let Some(workspace) = workspace else {
            return;
        };
        workspace.toggle_window_floating(window);
        if target_window_was_workspace_focus {
            if let Some(window_id) = window {
                workspace.activate_window(window_id);
            }
        }
        self.seat_focus_after_mutation();
        if let Some(workspace_id) = target_workspace_id {
            self.seat_focus_record_workspace_chain(workspace_id);
        }
    }

    pub fn toggle_window_sticky(&mut self, window: Option<&W::Id>) {
        self.request_refresh();
        if let Some(InteractiveMoveState::Moving(move_)) = &self.interactive_move {
            if window.is_none() || window == Some(move_.tile.window().id()) {
                return;
            }
        }

        let target = match window {
            Some(id) => id.clone(),
            None => match self.focus() {
                Some(win) => win.id().clone(),
                None => return,
            },
        };

        let target_is_active = self.focus().is_some_and(|win| win.id() == &target);

        if let Some(mon) = self
            .monitors_mut()
            .find(|mon| mon.has_sticky_window(&target))
        {
            mon.remove_sticky_window(&target, target_is_active);
            return;
        }

        if let MonitorSet::Normal { monitors, .. } = &mut self.monitor_set {
            for mon in monitors {
                if mon.add_sticky_window(&target, target_is_active) {
                    return;
                }
            }
        }
    }

    pub fn set_window_floating(&mut self, window: Option<&W::Id>, floating: bool) {
        self.request_refresh();
        if let Some(InteractiveMoveState::Moving(move_)) = &mut self.interactive_move {
            if window.is_none() || window == Some(move_.tile.window().id()) {
                if move_.is_floating != floating {
                    self.toggle_window_floating(window);
                }
                return;
            }
        }

        let target_workspace_id = window.and_then(|id| {
            self.workspaces()
                .find(|(_, _, ws)| ws.has_window(id))
                .map(|(_, _, ws)| ws.id())
        });
        let target_window_was_workspace_focus = match (target_workspace_id, window) {
            (Some(workspace_id), Some(window_id)) => {
                self.seat_focus_workspace_targets_window(workspace_id, window_id)
            }
            _ => false,
        };
        let workspace = if let Some(window) = window {
            self.workspaces_mut().find(|ws| ws.has_window(window))
        } else {
            self.active_workspace_mut()
        };

        let Some(workspace) = workspace else {
            return;
        };
        workspace.set_window_floating(window, floating);
        if target_window_was_workspace_focus {
            if let Some(window_id) = window {
                workspace.activate_window(window_id);
            }
        }
        self.seat_focus_after_mutation();
        if let Some(workspace_id) = target_workspace_id {
            self.seat_focus_record_workspace_chain(workspace_id);
        }
    }

    pub fn focus_floating(&mut self) {
        self.clear_sticky_focus();
        if self.focus_mode(true) {
            self.seat_focus_record_active_chain();
        }
    }

    pub fn focus_tiling(&mut self) {
        self.clear_sticky_focus();
        if self.focus_mode(false) {
            self.seat_focus_record_active_chain();
        }
    }

    pub fn switch_focus_floating_tiling(&mut self) {
        self.clear_sticky_focus();
        let Some(target_floating) = self
            .active_workspace()
            .map(|ws| ws.focus_mode_toggle_targets_floating())
        else {
            return;
        };
        if self.focus_mode(target_floating) {
            self.seat_focus_record_active_chain();
        }
    }

    fn focus_mode(&mut self, floating: bool) -> bool {
        if !floating
            && self.active_workspace().is_some_and(|ws| {
                ws.floating_is_active()
                    && ws
                        .active_window()
                        .is_some_and(|window| window.is_pending_windowed_fullscreen())
            })
        {
            // Match fullscreen focus constraints: don't move focus from an active
            // fullscreen floating container to tiling.
            return false;
        }

        if floating {
            let was_floating_active = self
                .active_workspace()
                .is_some_and(|ws| ws.floating_is_active());
            let Some(workspace) = self.active_workspace_mut() else {
                return false;
            };
            if workspace.restore_inactive_floating() {
                return true;
            }
            workspace.focus_floating();
            return !was_floating_active
                && self
                    .active_workspace()
                    .is_some_and(|ws| ws.floating_is_active());
        }

        let was_floating_active = self
            .active_workspace()
            .is_some_and(|ws| ws.floating_is_active());
        let Some(workspace) = self.active_workspace_mut() else {
            return false;
        };
        if let Some(restored) = workspace.restore_inactive_tiling() {
            return restored;
        }
        workspace.focus_tiling();
        was_floating_active
            && self
                .active_workspace()
                .is_some_and(|ws| !ws.floating_is_active())
    }

    pub fn move_window_to_scratchpad(&mut self, window: Option<&W::Id>) {
        if let Some(InteractiveMoveState::Moving(move_)) = &self.interactive_move {
            if window.is_none() || window == Some(move_.tile.window().id()) {
                return;
            }
        }

        let target = match window {
            Some(id) => id.clone(),
            None => match self.focus() {
                Some(win) => win.id().clone(),
                None => return,
            },
        };

        if self
            .scratchpad
            .tiles()
            .any(|tile| tile.window().id() == &target)
        {
            return;
        }

        match &mut self.monitor_set {
            MonitorSet::Normal { monitors, .. } => {
                let Some(monitor) = monitors.iter_mut().find(|mon| mon.has_window(&target)) else {
                    return;
                };

                let tile = monitor
                    .workspaces
                    .iter_mut()
                    .find(|ws| ws.has_window(&target))
                    .and_then(|ws| ws.take_tile_for_hiding_in_scratchpad(&target));

                let Some(tile) = tile else {
                    return;
                };
                self.scratchpad.hide_in_scratchpad(tile);

                if monitor.workspace_switch.is_none() {
                    monitor.clean_up_workspaces();
                }
            }
            MonitorSet::NoOutputs { workspaces } => {
                let tile = workspaces
                    .iter_mut()
                    .find(|ws| ws.has_window(&target))
                    .and_then(|ws| ws.take_tile_for_hiding_in_scratchpad(&target));

                let Some(tile) = tile else {
                    return;
                };
                self.scratchpad.hide_in_scratchpad(tile);
                workspaces.retain(|ws| ws.has_windows_or_persistent_identity());
            }
        }
        self.seat_focus_after_mutation();
    }

    pub fn scratchpad_show(&mut self) {
        let (active_ws_id, active_visible) = {
            let Some(workspace) = self.active_workspace() else {
                return;
            };
            let id = workspace.id();
            let visible = workspace
                .scratchpad_window_id()
                .map(|visible_id| (id, visible_id));
            (id, visible)
        };

        let visible_elsewhere = active_visible.or_else(|| {
            self.workspaces().find_map(|(_, _, ws)| {
                if ws.id() == active_ws_id {
                    return None;
                }
                let id = ws.scratchpad_window_id()?;
                Some((ws.id(), id))
            })
        });

        // Showing one that is already out puts it away again; showing one that is out on
        // another workspace brings it here; otherwise the next one hidden comes out. Three
        // moves between workspaces, which is all sway's scratchpad ever does.
        if let Some((ws_id, visible_id)) = visible_elsewhere {
            if ws_id == active_ws_id {
                let tile = self.active_workspace_mut().and_then(|workspace| {
                    workspace.take_tile_for_hiding_in_scratchpad(&visible_id)
                });
                if let Some(tile) = tile {
                    self.scratchpad.hide_in_scratchpad(tile);
                    self.seat_focus_after_mutation();
                }
                return;
            }

            let source_monitor_idx = self
                .find_workspace_location_by_id(ws_id)
                .map(|(idx, _)| idx);

            let tile = self
                .workspaces_mut()
                .find(|ws| ws.id() == ws_id)
                .and_then(|ws| ws.take_tile_for_scratchpad(&visible_id));

            if let (Some(monitor_idx), MonitorSet::Normal { monitors, .. }) =
                (source_monitor_idx, &mut self.monitor_set)
            {
                if monitors[monitor_idx].workspace_switch.is_none() {
                    monitors[monitor_idx].clean_up_workspaces();
                }
            }

            if let (Some(tile), Some(monitor)) = (tile, self.active_monitor()) {
                monitor.add_scratchpad_tile(tile, true);
            }
            self.seat_focus_after_mutation();
            return;
        }

        let Some(next) = self.scratchpad.next_scratchpad_window() else {
            return;
        };
        let tile = self.scratchpad.take_tile_for_scratchpad(&next);
        if let (Some(tile), Some(monitor)) = (tile, self.active_monitor()) {
            monitor.add_scratchpad_tile(tile, true);
            self.seat_focus_after_mutation();
        }
    }

    pub fn mark_focused(&mut self, mark: String, mode: MarkMode) {
        let Some(target) = self.active_mark_target_key() else {
            return;
        };

        let has_mark = self.node_has_mark(target, &mark);
        if matches!(mode, MarkMode::Toggle) && has_mark {
            self.remove_mark_from_node(target, &mark);
            return;
        }

        if matches!(mode, MarkMode::Replace) {
            self.clear_marks_on_node(target);
        }

        self.remove_mark_everywhere(&mark);
        self.add_mark_to_node(target, mark);
    }

    /// A representative window below the node carrying `mark`, if any.
    ///
    /// Marks name containers, including structural ones. Callers that only need an output can
    /// use the focused descendant without weakening the mark's actual node identity.
    pub fn window_id_with_mark(&self, mark: &str) -> Option<W::Id> {
        if let Some(InteractiveMoveState::Moving(move_)) = &self.interactive_move {
            if move_.tile.has_mark(mark) {
                return Some(move_.tile.window().id().clone());
            }
        }

        if let Some(id) = self.scratchpad.window_id_with_mark(mark) {
            return Some(id);
        }

        if let Some(id) = self
            .monitors()
            .find_map(|monitor| monitor.sticky_window_id_with_mark(mark))
        {
            return Some(id);
        }

        self.workspaces()
            .find_map(|(_, _, ws)| ws.window_id_with_mark(mark))
    }

    /// sway's `swap container with mark <mark>`.
    pub fn swap_window_with_mark(&mut self, mark: &str) -> bool {
        let Some(workspace) = self.active_workspace_mut() else {
            return false;
        };
        workspace.swap_selected_with_mark(mark)
    }

    /// i3's `unmark`: named, it takes that mark off whichever container holds it; bare, it
    /// clears every mark in the layout.
    ///
    /// The bare form really is the sweeping one — `sway/commands/unmark.c` walks every
    /// container, and it is the criteria in front of the command, which tiri has no
    /// equivalent of, that narrows it to one target. Clearing only the focused container's
    /// marks would leave no way to say the thing the command exists to say.
    pub fn unmark(&mut self, mark: Option<&str>) {
        match mark {
            Some(mark) => self.remove_mark_everywhere(mark),
            None => self.clear_marks_everywhere(),
        }
    }

    pub fn move_floating_window(
        &mut self,
        id: Option<&W::Id>,
        x: PositionChange,
        y: PositionChange,
        animate: bool,
    ) {
        self.request_refresh();
        if let Some(InteractiveMoveState::Moving(move_)) = &mut self.interactive_move {
            if id.is_none() || id == Some(move_.tile.window().id()) {
                return;
            }
        }

        let workspace = if let Some(id) = id {
            if let Some(mon) = self.monitors_mut().find(|mon| mon.has_sticky_window(id)) {
                mon.move_sticky_window(Some(id), x, y, animate);
                return;
            }

            self.workspaces_mut().find(|ws| ws.has_window(id))
        } else {
            if let Some(mon) = self.active_monitor() {
                if mon.sticky_is_active() {
                    mon.move_sticky_window(None, x, y, animate);
                    return;
                }
            }

            self.active_workspace_mut()
        };

        let Some(workspace) = workspace else {
            return;
        };
        workspace.move_floating_window(id, x, y, animate);
    }

    pub fn focus_output(&mut self, output: &Output) {
        let target_monitor_idx = match &self.monitor_set {
            MonitorSet::Normal { monitors, .. } => {
                monitors.iter().position(|mon| &mon.output == output)
            }
            MonitorSet::NoOutputs { .. } => None,
        };
        let Some(target_monitor_idx) = target_monitor_idx else {
            return;
        };

        if let MonitorSet::Normal {
            active_monitor_idx, ..
        } = &mut self.monitor_set
        {
            *active_monitor_idx = target_monitor_idx;
        }

        self.seat_focus_restore_output(output);
        self.seat_focus_record_active_chain();
    }

    fn focus_output_in_direction_internal(&mut self, output: &Output, direction: Direction) {
        let mut target_monitor_idx = None;
        let mut target_workspace_id = None;
        let mut target_has_tiling = false;

        if let MonitorSet::Normal {
            monitors,
            active_monitor_idx,
            ..
        } = &mut self.monitor_set
        {
            let Some(idx) = monitors.iter().position(|mon| &mon.output == output) else {
                return;
            };
            *active_monitor_idx = idx;

            let ws_idx = monitors[idx].active_workspace_idx;
            let ws = &mut monitors[idx].workspaces[ws_idx];
            target_has_tiling = ws.has_tiling_windows();
            if !target_has_tiling {
                ws.focus_workspace_node();
            }
            target_workspace_id = Some(ws.id());
            target_monitor_idx = Some(idx);
        }

        let Some(target_monitor_idx) = target_monitor_idx else {
            return;
        };

        if target_has_tiling {
            let Some(workspace_id) = target_workspace_id else {
                return;
            };

            let mut restored_tiling = false;
            if let Some((monitor_idx, workspace_idx)) =
                self.find_workspace_location_by_id(workspace_id)
            {
                if monitor_idx == target_monitor_idx {
                    if let MonitorSet::Normal { monitors, .. } = &mut self.monitor_set {
                        let ws = &mut monitors[monitor_idx].workspaces[workspace_idx];
                        restored_tiling = ws.restore_inactive_tiling().unwrap_or(false);
                    }
                }
            }

            if !restored_tiling {
                let mut focused_by_edge_target = false;
                if let Some((monitor_idx, workspace_idx)) =
                    self.find_workspace_location_by_id(workspace_id)
                {
                    if monitor_idx == target_monitor_idx {
                        if let MonitorSet::Normal { monitors, .. } = &mut self.monitor_set {
                            let ws = &mut monitors[monitor_idx].workspaces[workspace_idx];
                            focused_by_edge_target =
                                ws.focus_entry_from_output_direction(direction);
                        }
                    }
                }

                if focused_by_edge_target {
                    return;
                }

                if let Some((monitor_idx, workspace_idx)) =
                    self.find_workspace_location_by_id(workspace_id)
                {
                    if monitor_idx == target_monitor_idx {
                        if let MonitorSet::Normal { monitors, .. } = &mut self.monitor_set {
                            let ws = &mut monitors[monitor_idx].workspaces[workspace_idx];
                            ws.focus_tiling();
                        }
                    }
                }
            }
        }
    }

    pub fn move_to_output(
        &mut self,
        window: Option<&W::Id>,
        output: &Output,
        target_ws_idx: Option<usize>,
        activate: ActivateWindow,
    ) {
        self.request_refresh();
        if let Some(InteractiveMoveState::Moving(move_)) = &mut self.interactive_move {
            if window.is_none() || window == Some(move_.tile.window().id()) {
                return;
            }
        }

        let focused_id = self.focus().map(|win| win.id().clone());
        let sticky_target = window.cloned().or_else(|| {
            focused_id.as_ref().and_then(|id| {
                self.monitors()
                    .any(|mon| mon.has_sticky_window(id))
                    .then(|| id.clone())
            })
        });
        let target_is_focused = focused_id
            .as_ref()
            .is_some_and(|id| Some(id) == sticky_target.as_ref());

        if let MonitorSet::Normal {
            monitors,
            active_monitor_idx,
            ..
        } = &mut self.monitor_set
        {
            if let Some(sticky_id) = sticky_target {
                if let Some(src_idx) = monitors
                    .iter()
                    .position(|mon| mon.has_sticky_window(&sticky_id))
                {
                    let Some(new_idx) = monitors.iter().position(|mon| &mon.output == output)
                    else {
                        return;
                    };
                    if src_idx == new_idx {
                        return;
                    }

                    let activate = activate.map_smart(|| target_is_focused);
                    let activate = if activate {
                        ActivateWindow::Yes
                    } else {
                        ActivateWindow::No
                    };
                    let activate_flag = matches!(activate, ActivateWindow::Yes);

                    let was_active = monitors[src_idx].sticky_is_active()
                        && monitors[src_idx]
                            .sticky_active_window_id()
                            .is_some_and(|id| id == &sticky_id);

                    let mut removed = monitors[src_idx]
                        .take_sticky_window(&sticky_id)
                        .expect("sticky window should exist");
                    removed.tile.set_sticky(true);
                    if was_active {
                        monitors[src_idx].clear_sticky_focus();
                    }

                    let mon = &mut monitors[new_idx];
                    mon.add_sticky_tile(removed.tile, activate_flag);
                    if activate_flag {
                        *active_monitor_idx = new_idx;
                    }

                    return;
                }
            }

            let Some(new_idx) = monitors.iter().position(|mon| &mon.output == output) else {
                return;
            };

            let (mon_idx, ws_idx) = if let Some(window) = window {
                let Some(found) = monitors.iter().enumerate().find_map(|(mon_idx, mon)| {
                    mon.workspaces
                        .iter()
                        .position(|ws| ws.has_window(window))
                        .map(|ws_idx| (mon_idx, ws_idx))
                }) else {
                    return;
                };
                found
            } else {
                let mon_idx = *active_monitor_idx;
                let mon = &monitors[mon_idx];
                (mon_idx, mon.active_workspace_idx)
            };

            let workspace_idx = target_ws_idx.unwrap_or(monitors[new_idx].active_workspace_idx);
            if mon_idx == new_idx && ws_idx == workspace_idx {
                return;
            }

            let mon = &monitors[new_idx];
            if mon.workspaces.len() <= workspace_idx {
                return;
            }

            let ws_id = mon.workspaces[workspace_idx].id();

            let mon = &mut monitors[mon_idx];
            let activate = activate.map_smart(|| {
                window.is_none_or(|win| {
                    mon_idx == *active_monitor_idx
                        && mon.active_window().map(|win| win.id()) == Some(win)
                })
            });
            let activate = if activate {
                ActivateWindow::Yes
            } else {
                ActivateWindow::No
            };

            let ws = &mut mon.workspaces[ws_idx];
            let Some(window) = window.or_else(|| ws.active_window().map(|win| win.id())) else {
                return;
            };
            let window = window.clone();

            let transaction = Transaction::new();
            let mut removed = ws.remove_tile(&window, transaction);

            removed.prepare_for_workspace_move();
            removed.tile.stop_move_animations();

            let mon = &mut monitors[new_idx];
            mon.add_tile(
                removed.tile,
                MonitorAddWindowTarget::Workspace {
                    id: ws_id,
                    column_idx: None,
                },
                activate,
                true,
                removed.width,
                removed.is_floating,
                None,
            );
            if activate.map_smart(|| false) {
                *active_monitor_idx = new_idx;
            }

            let mon = &mut monitors[mon_idx];
            if mon.workspace_switch.is_none() {
                monitors[mon_idx].clean_up_workspaces();
            }
        }
        self.seat_focus_after_mutation();
    }

    pub fn move_container_to_output(
        &mut self,
        output: &Output,
        target_ws_idx: Option<usize>,
        activate: bool,
    ) {
        self.move_tiling_target_to_output(output, target_ws_idx, activate, false);
    }

    fn move_tiling_target_to_output(
        &mut self,
        output: &Output,
        target_ws_idx: Option<usize>,
        activate: bool,
        root_child: bool,
    ) {
        if let MonitorSet::Normal {
            monitors,
            active_monitor_idx,
            ..
        } = &mut self.monitor_set
        {
            let new_idx = monitors
                .iter()
                .position(|mon| &mon.output == output)
                .unwrap();

            let current = &mut monitors[*active_monitor_idx];
            let ws = current.active_workspace();

            if ws.floating_is_active() {
                self.move_to_output(None, output, None, ActivateWindow::Smart);
                return;
            }

            let subtree = if root_child {
                ws.remove_active_root_tiling_subtree()
            } else {
                ws.remove_active_tiling_subtree()
            };
            let Some(mut subtree) = subtree else {
                return;
            };
            subtree.prepare_for_workspace_move();

            let workspace_idx = target_ws_idx
                .unwrap_or(monitors[new_idx].active_workspace_idx)
                .min(monitors[new_idx].workspaces.len() - 1);
            self.add_root_tiling_subtree_by_idx(new_idx, workspace_idx, subtree, activate);
        }
        self.seat_focus_after_mutation();
    }

    pub fn move_column_to_output(
        &mut self,
        output: &Output,
        target_ws_idx: Option<usize>,
        activate: bool,
    ) {
        self.move_tiling_target_to_output(output, target_ws_idx, activate, true);
    }

    pub fn move_workspace_to_output(&mut self, output: &Output) -> bool {
        let MonitorSet::Normal {
            monitors,
            active_monitor_idx,
            ..
        } = &self.monitor_set
        else {
            return false;
        };

        let idx = monitors[*active_monitor_idx].active_workspace_idx;
        self.move_workspace_to_output_by_index(idx, None, output)
    }

    pub fn move_workspace_to_output_by_id(
        &mut self,
        workspace_id: WorkspaceId,
        new_output: &Output,
    ) -> bool {
        self.move_workspace_to_output_by_workspace_id(workspace_id, new_output)
    }

    pub fn move_workspace_to_output_by_workspace_id(
        &mut self,
        workspace_id: WorkspaceId,
        new_output: &Output,
    ) -> bool {
        let MonitorSet::Normal { monitors, .. } = &self.monitor_set else {
            return false;
        };

        let Some((mon_idx, old_idx)) = monitors.iter().enumerate().find_map(|(mon_idx, mon)| {
            mon.workspaces
                .iter()
                .position(|ws| ws.id() == workspace_id)
                .map(|idx| (mon_idx, idx))
        }) else {
            return false;
        };

        let old_output = monitors[mon_idx].output.clone();
        self.move_workspace_to_output_by_index(old_idx, Some(old_output), new_output)
    }

    pub fn move_workspace_to_output_by_index(
        &mut self,
        old_idx: usize,
        old_output: Option<Output>,
        new_output: &Output,
    ) -> bool {
        let MonitorSet::Normal {
            monitors,
            active_monitor_idx,
            ..
        } = &mut self.monitor_set
        else {
            return false;
        };

        let current_idx = if let Some(old_output) = old_output {
            monitors
                .iter()
                .position(|mon| mon.output == old_output)
                .unwrap()
        } else {
            *active_monitor_idx
        };
        let target_idx = monitors
            .iter()
            .position(|mon| mon.output == *new_output)
            .unwrap();

        let current = &mut monitors[current_idx];

        if current.workspaces.len() <= old_idx {
            return false;
        }

        // Do not do anything if the output is already correct
        if current_idx == target_idx {
            // Just update the original output since this is an explicit movement action.
            current.workspaces[old_idx].original_output = OutputId::new(&current.output);

            return false;
        }

        // Only switch active monitor if the workspace to be moved is the currently focused one on
        // the current monitor.
        let activate =
            current_idx == *active_monitor_idx && old_idx == current.active_workspace_idx;

        let mut ws = current.remove_workspace_by_idx(old_idx);
        ws.original_output = OutputId::new(new_output);

        let target = &mut monitors[target_idx];
        target.insert_workspace(ws, target.active_workspace_idx + 1, activate);

        if activate {
            *active_monitor_idx = target_idx;
        }

        self.seat_focus_after_mutation();
        activate
    }

    pub fn set_fullscreen(&mut self, id: &W::Id, is_fullscreen: bool) {
        self.request_refresh();
        // Check if this is a request to unset the windowed fullscreen state.
        if !is_fullscreen {
            let mut handled = false;
            self.with_windows_mut(|window, _| {
                if window.id() == id && window.is_pending_windowed_fullscreen() {
                    window.request_windowed_fullscreen(false);
                    handled = true;
                }
            });
            if handled {
                return;
            }
        }

        if self.interactive_move_targets_window(id) {
            return;
        }

        for ws in self.workspaces_mut() {
            if ws.has_window(id) {
                ws.set_fullscreen(id, is_fullscreen);
                return;
            }
        }
    }

    pub fn toggle_fullscreen(&mut self, id: &W::Id) {
        self.request_refresh();
        if self.interactive_move_targets_window(id) {
            return;
        }

        for ws in self.workspaces_mut() {
            if ws.has_window(id) {
                ws.toggle_fullscreen(id);
                return;
            }
        }
    }

    pub fn toggle_fullscreen_for_active_command(&mut self, id: &W::Id) {
        self.request_refresh();
        if self.interactive_move_targets_window(id) {
            return;
        }

        for ws in self.workspaces_mut() {
            if ws.has_window(id) {
                ws.toggle_fullscreen_for_command(id);
                return;
            }
        }
    }

    pub fn set_windowed_fullscreen(&mut self, id: &W::Id, is_fullscreen: bool) {
        self.request_refresh();
        if self.interactive_move_targets_window(id) {
            return;
        }

        let Some((_, window)) = self.windows().find(|(_, win)| win.id() == id) else {
            return;
        };

        if !is_fullscreen && window.is_pending_windowed_fullscreen() {
            self.with_windows_mut(|window, _| {
                if window.id() == id {
                    window.request_windowed_fullscreen(false);
                }
            });
            return;
        }

        if is_fullscreen {
            let is_floating = self
                .workspaces()
                .find(|(_, _, ws)| ws.has_window(id))
                .is_some_and(|(_, _, ws)| ws.is_floating(id));
            if is_floating {
                self.set_fullscreen(id, true);
                return;
            }
        } else if window.pending_sizing_mode().is_fullscreen()
            || window.sizing_mode().is_fullscreen()
        {
            self.set_fullscreen(id, false);
            return;
        }

        self.with_windows_mut(|window, _| {
            if window.id() == id {
                window.request_windowed_fullscreen(is_fullscreen);
            }
        });
    }

    pub fn toggle_windowed_fullscreen(&mut self, id: &W::Id) {
        self.toggle_fullscreen(id);
    }

    fn interactive_move_targets_window(&self, id: &W::Id) -> bool {
        match &self.interactive_move {
            Some(InteractiveMoveState::Starting { window_id, .. }) => window_id == id,
            Some(InteractiveMoveState::Moving(move_)) => move_.tile.window().id() == id,
            Some(InteractiveMoveState::MovingContainer(move_)) => &move_.window_id == id,
            None => false,
        }
    }

    pub fn workspace_switch_gesture_begin(&mut self, output: &Output, is_touchpad: bool) {
        let monitors = match &mut self.monitor_set {
            MonitorSet::Normal { monitors, .. } => monitors,
            MonitorSet::NoOutputs { .. } => unreachable!(),
        };

        for monitor in monitors {
            // Cancel the gesture on other outputs.
            if &monitor.output != output {
                monitor.workspace_switch_gesture_end(None);
                continue;
            }

            monitor.workspace_switch_gesture_begin(is_touchpad);
        }
    }

    pub fn workspace_switch_gesture_update(
        &mut self,
        delta_y: f64,
        timestamp: Duration,
        is_touchpad: bool,
    ) -> Option<Option<Output>> {
        let monitors = match &mut self.monitor_set {
            MonitorSet::Normal { monitors, .. } => monitors,
            MonitorSet::NoOutputs { .. } => return None,
        };

        for monitor in monitors {
            if let Some(refresh) =
                monitor.workspace_switch_gesture_update(delta_y, timestamp, is_touchpad)
            {
                if refresh {
                    return Some(Some(monitor.output.clone()));
                } else {
                    return Some(None);
                }
            }
        }

        None
    }

    pub fn workspace_switch_gesture_end(&mut self, is_touchpad: Option<bool>) -> Option<Output> {
        let monitors = match &mut self.monitor_set {
            MonitorSet::Normal { monitors, .. } => monitors,
            MonitorSet::NoOutputs { .. } => return None,
        };

        for monitor in monitors {
            if monitor.workspace_switch_gesture_end(is_touchpad) {
                return Some(monitor.output.clone());
            }
        }

        None
    }

    pub fn horizontal_view_gesture_begin(
        &mut self,
        output: &Output,
        workspace_idx: Option<usize>,
        is_touchpad: bool,
    ) {
        let monitors = match &mut self.monitor_set {
            MonitorSet::Normal { monitors, .. } => monitors,
            MonitorSet::NoOutputs { .. } => unreachable!(),
        };

        for monitor in monitors {
            for (idx, ws) in monitor.workspaces.iter_mut().enumerate() {
                // Cancel the gesture on other workspaces.
                if &monitor.output != output
                    || idx != workspace_idx.unwrap_or(monitor.active_workspace_idx)
                {
                    ws.horizontal_view_gesture_end(None);
                    continue;
                }

                ws.horizontal_view_gesture_begin(is_touchpad);
            }
        }
    }

    pub fn horizontal_view_gesture_update(
        &mut self,
        delta_x: f64,
        timestamp: Duration,
        is_touchpad: bool,
    ) -> Option<Option<Output>> {
        let zoom = self.overview_zoom();
        let delta_x = delta_x / zoom;

        let monitors = match &mut self.monitor_set {
            MonitorSet::Normal { monitors, .. } => monitors,
            MonitorSet::NoOutputs { .. } => return None,
        };

        for monitor in monitors {
            for ws in &mut monitor.workspaces {
                if let Some(refresh) =
                    ws.horizontal_view_gesture_update(delta_x, timestamp, is_touchpad)
                {
                    if refresh {
                        return Some(Some(monitor.output.clone()));
                    } else {
                        return Some(None);
                    }
                }
            }
        }

        None
    }

    pub fn horizontal_view_gesture_end(&mut self, is_touchpad: Option<bool>) -> Option<Output> {
        let monitors = match &mut self.monitor_set {
            MonitorSet::Normal { monitors, .. } => monitors,
            MonitorSet::NoOutputs { .. } => return None,
        };

        for monitor in monitors {
            for ws in &mut monitor.workspaces {
                if ws.horizontal_view_gesture_end(is_touchpad) {
                    return Some(monitor.output.clone());
                }
            }
        }

        None
    }

    pub fn overview_gesture_begin(&mut self) {
        self.overview_open = true;

        let value = self.overview_progress.take().map_or(0., |p| p.value());
        let gesture = OverviewGesture {
            tracker: SwipeTracker::new(),
            start: value,
            value,
        };
        self.overview_progress = Some(OverviewProgress::Gesture(gesture));

        self.set_monitors_overview_state();
    }

    pub fn overview_gesture_update(&mut self, delta_y: f64, timestamp: Duration) -> Option<bool> {
        let Some(OverviewProgress::Gesture(gesture)) = &mut self.overview_progress else {
            return None;
        };

        gesture.tracker.push(delta_y, timestamp);

        let total_height = OVERVIEW_GESTURE_MOVEMENT;
        let pos = gesture.tracker.pos() / total_height;
        let new_value = gesture.start + pos;
        let new_value = OVERVIEW_GESTURE_RUBBER_BAND.clamp(0., 1., new_value);

        if gesture.value == new_value {
            return Some(false);
        }

        gesture.value = new_value;
        self.set_monitors_overview_state();

        Some(true)
    }

    pub fn overview_gesture_end(&mut self) -> bool {
        let Some(OverviewProgress::Gesture(gesture)) = &mut self.overview_progress else {
            return false;
        };

        // Take into account any idle time between the last event and now.
        let now = self.clock.now_unadjusted();
        gesture.tracker.push(0., now);

        let total_height = OVERVIEW_GESTURE_MOVEMENT;

        let mut velocity = gesture.tracker.velocity() / total_height;
        let current_pos = gesture.tracker.pos() / total_height;
        let pos = gesture.tracker.projected_end_pos() / total_height;

        let new_value = gesture.start + pos;
        let new_value = new_value.clamp(0., 1.).round();

        velocity *=
            OVERVIEW_GESTURE_RUBBER_BAND.clamp_derivative(0., 1., gesture.start + current_pos);

        self.overview_open = new_value == 1.;
        self.overview_progress = Some(OverviewProgress::Animation(Animation::new(
            self.clock.clone(),
            gesture.value,
            new_value,
            velocity,
            self.options.animations.overview_open_close.0,
        )));

        self.set_monitors_overview_state();

        true
    }

    pub fn interactive_move_begin(
        &mut self,
        window_id: W::Id,
        output: &Output,
        start_pos_within_output: Point<f64, Logical>,
    ) -> bool {
        self.request_refresh();
        if self.interactive_move.is_some() {
            return false;
        }

        let mut found = None;
        for mon in self.monitors() {
            if let Some((ws, ws_geo)) = mon
                .workspaces_with_render_geo()
                .find(|(ws, _)| ws.has_window(&window_id))
            {
                let Some((tile, tile_offset, _visible)) = ws
                    .tiles_with_render_positions()
                    .find(|(tile, _, _)| tile.window().id() == &window_id)
                else {
                    continue;
                };
                let window_offset = tile.window_loc();
                let window_size = tile.window_size();
                let is_floating = ws.is_floating(&window_id);
                found = Some((
                    mon,
                    ws_geo,
                    tile_offset,
                    window_offset,
                    window_size,
                    is_floating,
                ));
                break;
            }

            if mon.has_sticky_window(&window_id) {
                let Some(ws_geo) = mon.active_workspace_render_geo() else {
                    continue;
                };
                let Some((tile, tile_offset)) = mon.sticky_tile_with_render_position(&window_id)
                else {
                    continue;
                };
                let window_offset = tile.window_loc();
                let window_size = tile.window_size();
                found = Some((mon, ws_geo, tile_offset, window_offset, window_size, true));
                break;
            }
        }

        let Some((mon, ws_geo, tile_offset, window_offset, window_size, _is_floating)) = found
        else {
            return false;
        };

        if mon.output() != output {
            return false;
        }

        let zoom = mon.overview_zoom();

        let tile_pos = ws_geo.loc + tile_offset.upscale(zoom);

        let pointer_offset_within_window =
            start_pos_within_output - tile_pos - window_offset.upscale(zoom);
        let window_size = window_size.upscale(zoom);
        let pointer_ratio_within_window = (
            f64::clamp(pointer_offset_within_window.x / window_size.w, 0., 1.),
            f64::clamp(pointer_offset_within_window.y / window_size.h, 0., 1.),
        );

        self.interactive_move = Some(InteractiveMoveState::Starting {
            window_id,
            pointer_delta: Point::from((0., 0.)),
            pointer_ratio_within_window,
        });

        for mon in self.monitors_mut() {
            mon.dnd_scroll_gesture_begin();
        }

        true
    }

    /// Where a window being interactively moved currently lives.
    ///
    /// Sticky windows hang off a monitor rather than a workspace, so every step of a drag
    /// has to ask a different owner for the same information. Resolving the host once keeps
    /// that branch out of the drag logic.
    fn move_host(&self, window: &W::Id) -> MoveHost {
        match self
            .monitors()
            .position(|mon| mon.has_sticky_window(window))
        {
            Some(idx) => MoveHost::Sticky(idx),
            None => MoveHost::Workspace,
        }
    }

    /// Set the dragged tile's rubber-band offset. Returns false when the window is gone.
    fn set_interactive_move_offset(
        &mut self,
        host: MoveHost,
        window: &W::Id,
        offset: Point<f64, Logical>,
    ) -> bool {
        let tile = match host {
            MoveHost::Sticky(idx) => self
                .monitors_mut()
                .nth(idx)
                .and_then(|mon| mon.sticky_tiles_mut().find(|t| t.window().id() == window)),
            MoveHost::Workspace => self
                .workspaces_mut()
                .find(|ws| ws.has_window(window))
                .and_then(|ws| ws.tiles_mut().find(|t| t.window().id() == window)),
        };

        let Some(tile) = tile else {
            return false;
        };
        tile.interactive_move_offset = offset;
        true
    }

    /// Whether the window sits in a floating container that is dragged as a whole.
    fn move_container_allows_splits(&self, host: MoveHost, window: &W::Id) -> bool {
        match host {
            MoveHost::Sticky(idx) => self
                .monitors()
                .nth(idx)
                .is_some_and(|mon| mon.sticky_container_allows_splits(window)),
            MoveHost::Workspace => self
                .workspaces()
                .map(|(_, _, ws)| ws)
                .find(|ws| ws.has_window(window))
                .is_some_and(|ws| ws.floating_container_allows_splits(window)),
        }
    }

    /// Position of the floating container holding the window.
    fn move_container_pos(&self, host: MoveHost, window: &W::Id) -> Option<Point<f64, Logical>> {
        match host {
            MoveHost::Sticky(idx) => self
                .monitors()
                .nth(idx)
                .and_then(|mon| mon.sticky_container_pos(window)),
            MoveHost::Workspace => self
                .workspaces()
                .map(|(_, _, ws)| ws)
                .find(|ws| ws.has_window(window))
                .and_then(|ws| ws.floating_container_pos(window)),
        }
    }

    pub fn interactive_move_update(
        &mut self,
        window: &W::Id,
        delta: Point<f64, Logical>,
        output: Output,
        pointer_pos_within_output: Point<f64, Logical>,
    ) -> bool {
        self.request_refresh();
        let Some(state) = self.interactive_move.take() else {
            return false;
        };

        match state {
            InteractiveMoveState::Starting {
                window_id,
                mut pointer_delta,
                pointer_ratio_within_window,
            } => {
                if window_id != *window {
                    self.interactive_move = Some(InteractiveMoveState::Starting {
                        window_id,
                        pointer_delta,
                        pointer_ratio_within_window,
                    });
                    return false;
                }

                let zoom = self.overview_zoom();
                let delta = delta.downscale(zoom);

                pointer_delta += delta;

                let (cx, cy) = (pointer_delta.x, pointer_delta.y);
                let sq_dist = cx * cx + cy * cy;

                let factor = RubberBand {
                    stiffness: 1.0,
                    limit: 0.5,
                }
                .band(sq_dist / INTERACTIVE_MOVE_START_THRESHOLD);

                let host = self.move_host(&window_id);
                let sticky_monitor_idx = match host {
                    MoveHost::Sticky(idx) => Some(idx),
                    MoveHost::Workspace => None,
                };

                let (is_floating, workspace_config) = match host {
                    MoveHost::Sticky(_) => (true, None),
                    MoveHost::Workspace => {
                        let Some(ws) = self
                            .workspaces()
                            .map(|(_, _, ws)| ws)
                            .find(|ws| ws.has_window(&window_id))
                        else {
                            return false;
                        };
                        let workspace_config = ws.layout_config().cloned().map(|c| (ws.id(), c));
                        (ws.is_floating(&window_id), workspace_config)
                    }
                };
                let floating_grouped =
                    is_floating && self.move_container_allows_splits(host, &window_id);
                if !self.set_interactive_move_offset(
                    host,
                    &window_id,
                    pointer_delta.upscale(factor),
                ) {
                    return false;
                }

                // Put it back to be able to easily return.
                self.interactive_move = Some(InteractiveMoveState::Starting {
                    window_id: window_id.clone(),
                    pointer_delta,
                    pointer_ratio_within_window,
                });

                if !is_floating && sq_dist < INTERACTIVE_MOVE_START_THRESHOLD {
                    return true;
                }

                if floating_grouped {
                    // The whole container moves, so the window itself stops rubber-banding.
                    if !self.set_interactive_move_offset(host, &window_id, Point::from((0., 0.))) {
                        self.interactive_move = None;
                        return false;
                    }
                    // The Starting state was put back above, so bailing out here leaves the
                    // drag exactly as it was.
                    let Some(start_container_pos) = self.move_container_pos(host, &window_id)
                    else {
                        return false;
                    };

                    self.interactive_move = Some(InteractiveMoveState::MovingContainer(
                        InteractiveMoveContainerData {
                            window_id,
                            output: output.clone(),
                            pointer_pos_within_output,
                            start_pointer_pos_within_output: pointer_pos_within_output,
                            start_container_pos,
                        },
                    ));
                    return true;
                }

                let output_config = self
                    .monitors()
                    .find(|mon| mon.output() == &output)
                    .and_then(|mon| mon.layout_config().cloned());

                // If the pointer is currently on the window's own output, then we can animate the
                // window movement from its current (rubberbanded and possibly moved away) position
                // to the pointer. Otherwise, we just teleport it as the layout code is not aware
                // of monitor positions.
                //
                // FIXME: when and if the layout code knows about monitor positions, this will be
                // potentially animatable.
                let mut tile_pos = None;
                if let Some((mon, (ws, ws_geo))) = self.monitors().find_map(|mon| {
                    mon.workspaces_with_render_geo()
                        .find(|(ws, _)| ws.has_window(window))
                        .map(|rv| (mon, rv))
                }) {
                    if mon.output() == &output {
                        let (_, tile_offset, _) = ws
                            .tiles_with_render_positions()
                            .find(|(tile, _, _)| tile.window().id() == window)
                            .unwrap();

                        let zoom = mon.overview_zoom();
                        tile_pos = Some((ws_geo.loc + tile_offset.upscale(zoom), zoom));
                    }
                } else if let Some(mon_idx) = sticky_monitor_idx {
                    let mon = self.monitors().nth(mon_idx).unwrap();
                    if mon.output() == &output {
                        if let Some(ws_geo) = mon.active_workspace_render_geo() {
                            if let Some((_, tile_offset)) =
                                mon.sticky_tile_with_render_position(window)
                            {
                                let zoom = mon.overview_zoom();
                                tile_pos = Some((ws_geo.loc + tile_offset.upscale(zoom), zoom));
                            }
                        }
                    }
                }

                // Clear it before calling remove_window() to avoid running interactive_move_end()
                // in the middle of interactive_move_update() and the confusion that causes.
                self.interactive_move = None;

                let was_sticky = sticky_monitor_idx.is_some();

                let (origin_workspace, swap_origin) = if let Some(mon_idx) = sticky_monitor_idx {
                    let mon = self.monitors().nth(mon_idx).unwrap();
                    (mon.active_workspace_ref().id(), None)
                } else {
                    // Unset fullscreen before removing the tile. This will restore its size
                    // properly, and move it to floating if needed, so we don't have to deal with
                    // that here.
                    let ws = self
                        .workspaces_mut()
                        .find(|ws| ws.has_window(&window_id))
                        .unwrap();
                    ws.set_fullscreen(window, false);

                    let origin_workspace = ws.id();
                    let swap_origin = if is_floating {
                        None
                    } else {
                        ws.tiling_insert_parent_info(&window_id)
                    };
                    (origin_workspace, swap_origin)
                };
                let RemovedTile {
                    mut tile,
                    width,
                    is_floating,
                } = self.remove_window(window, Transaction::new()).unwrap();

                if was_sticky {
                    tile.set_sticky(true);
                }

                tile.stop_move_animations();
                tile.interactive_move_offset = Point::from((0., 0.));
                tile.window().output_enter(&output);
                tile.window().set_preferred_scale_transform(
                    output.current_scale(),
                    output.current_transform(),
                );

                let view_size = output_size(&output);
                let scale = output.current_scale().fractional_scale();
                let options = Options::clone(&self.options)
                    .with_merged_layout(output_config.as_ref())
                    .with_merged_layout(workspace_config.as_ref().map(|(_, c)| c))
                    .adjusted_for_scale(scale);
                tile.update_config(view_size, scale, Rc::new(options));
                if !tile.window().pending_sizing_mode().is_normal() {
                    tile.request_tile_size(tile.tile_size(), !self.options.animations.off, None);
                }

                if !is_floating {
                    // Animate to semitransparent.
                    tile.animate_alpha(
                        1.,
                        INTERACTIVE_MOVE_ALPHA,
                        self.options.animations.window_movement.0,
                    );
                    tile.hold_alpha_animation_after_done();
                }

                let mut data = InteractiveMoveData {
                    tile,
                    output,
                    pointer_pos_within_output,
                    width,
                    is_floating,
                    was_sticky,
                    pointer_ratio_within_window,
                    output_config,
                    workspace_config,
                    swap_origin,
                    origin_workspace,
                };

                if let Some((tile_pos, zoom)) = tile_pos {
                    let new_tile_pos = data.tile_render_location(zoom);
                    data.tile
                        .animate_move_from((tile_pos - new_tile_pos).downscale(zoom));
                }

                self.interactive_move = Some(InteractiveMoveState::Moving(data));
            }
            InteractiveMoveState::Moving(mut move_) => {
                if window != move_.tile.window().id() {
                    self.interactive_move = Some(InteractiveMoveState::Moving(move_));
                    return false;
                }

                let mut ws_id = None;
                if let Some(mon) = self.monitor_for_output(&output) {
                    let (insert_ws, _) = mon.insert_position(move_.pointer_pos_within_output);
                    if let InsertWorkspace::Existing(id) = insert_ws {
                        ws_id = Some(id);
                    }
                }

                // If moved over a different workspace, reset the config override.
                let mut update_config = false;
                if let Some((id, _)) = &move_.workspace_config {
                    if Some(*id) != ws_id {
                        move_.workspace_config = None;
                        update_config = true;
                    }
                }

                if output != move_.output {
                    move_.tile.window().output_leave(&move_.output);
                    move_.tile.window().output_enter(&output);
                    move_.tile.window().set_preferred_scale_transform(
                        output.current_scale(),
                        output.current_transform(),
                    );
                    move_.output = output.clone();
                    self.focus_output(&output);

                    move_.output_config = self
                        .monitor_for_output(&output)
                        .and_then(|mon| mon.layout_config().cloned());

                    update_config = true;
                }

                if update_config {
                    let view_size = output_size(&output);
                    let scale = output.current_scale().fractional_scale();
                    let options = Options::clone(&self.options)
                        .with_merged_layout(move_.output_config.as_ref())
                        .with_merged_layout(move_.workspace_config.as_ref().map(|(_, c)| c))
                        .adjusted_for_scale(scale);
                    move_.tile.update_config(view_size, scale, Rc::new(options));
                }

                move_.pointer_pos_within_output = pointer_pos_within_output;

                self.interactive_move = Some(InteractiveMoveState::Moving(move_));
            }
            InteractiveMoveState::MovingContainer(mut move_) => {
                if window != &move_.window_id {
                    self.interactive_move = Some(InteractiveMoveState::MovingContainer(move_));
                    return false;
                }

                if output != move_.output {
                    self.interactive_move = Some(InteractiveMoveState::MovingContainer(move_));
                    return false;
                }

                move_.pointer_pos_within_output = pointer_pos_within_output;

                let zoom = self.overview_zoom();
                let delta = (move_.pointer_pos_within_output
                    - move_.start_pointer_pos_within_output)
                    .downscale(zoom);
                let new_pos = move_.start_container_pos + delta;

                let moved_sticky = {
                    if let Some(mon) = self
                        .monitors_mut()
                        .find(|mon| mon.has_sticky_window(&move_.window_id))
                    {
                        mon.move_sticky_container_for_window_to(&move_.window_id, new_pos);
                        true
                    } else {
                        false
                    }
                };

                if !moved_sticky {
                    if let Some(ws) = self
                        .workspaces_mut()
                        .find(|ws| ws.has_window(&move_.window_id))
                    {
                        ws.move_floating_container_for_window_to(&move_.window_id, new_pos);
                    }
                }

                self.interactive_move = Some(InteractiveMoveState::MovingContainer(move_));
            }
        }

        true
    }

    pub fn interactive_move_end(&mut self, window: &W::Id) {
        self.request_refresh();
        let Some(move_) = &self.interactive_move else {
            return;
        };

        let move_ = match move_ {
            InteractiveMoveState::Starting { window_id, .. } => {
                if window_id != window {
                    return;
                }

                let Some(InteractiveMoveState::Starting { window_id, .. }) =
                    self.interactive_move.take()
                else {
                    unreachable!()
                };

                for mon in self.monitors_mut() {
                    mon.dnd_scroll_gesture_end();
                }

                for ws in self.workspaces_mut() {
                    if let Some(tile) = ws.tiles_mut().find(|tile| *tile.window().id() == window_id)
                    {
                        let offset = tile.interactive_move_offset;
                        tile.interactive_move_offset = Point::from((0., 0.));
                        tile.animate_move_from(offset);
                    }

                    // Unlock the view on the workspaces, but if the moved window was active,
                    // preserve that.
                    let moved_tile_was_active =
                        ws.active_window().is_some_and(|win| *win.id() == window_id);

                    if moved_tile_was_active {
                        ws.activate_window(&window_id);
                    }
                }

                for mon in self.monitors_mut() {
                    if let Some(tile) = mon
                        .sticky_tiles_mut()
                        .find(|tile| *tile.window().id() == window_id)
                    {
                        let offset = tile.interactive_move_offset;
                        tile.interactive_move_offset = Point::from((0., 0.));
                        tile.animate_move_from(offset);
                    }
                }

                return;
            }
            InteractiveMoveState::Moving(move_) => move_,
            InteractiveMoveState::MovingContainer(move_) => {
                if window != &move_.window_id {
                    return;
                }

                let Some(InteractiveMoveState::MovingContainer(move_)) =
                    self.interactive_move.take()
                else {
                    unreachable!()
                };

                for mon in self.monitors_mut() {
                    mon.dnd_scroll_gesture_end();
                }

                if let Some(ws) = self
                    .workspaces_mut()
                    .find(|ws| ws.has_window(&move_.window_id))
                {
                    if let Some(tile) = ws
                        .tiles_mut()
                        .find(|tile| *tile.window().id() == move_.window_id)
                    {
                        tile.interactive_move_offset = Point::from((0., 0.));
                    }
                }

                if let Some(mon) = self
                    .monitors_mut()
                    .find(|mon| mon.has_sticky_window(&move_.window_id))
                {
                    if let Some(tile) = mon
                        .sticky_tiles_mut()
                        .find(|tile| *tile.window().id() == move_.window_id)
                    {
                        tile.interactive_move_offset = Point::from((0., 0.));
                    }
                }

                return;
            }
        };

        if window != move_.tile.window().id() {
            return;
        }

        let Some(InteractiveMoveState::Moving(mut move_)) = self.interactive_move.take() else {
            unreachable!()
        };

        for mon in self.monitors_mut() {
            mon.dnd_scroll_gesture_end();
        }

        // Animate the tile back to opaque.
        if !move_.is_floating {
            move_.tile.animate_alpha(
                INTERACTIVE_MOVE_ALPHA,
                1.,
                self.options.animations.window_movement.0,
            );
        }

        // Dragging in the overview shouldn't switch the workspace and so on.
        let allow_to_activate_workspace = !self.overview_open;

        match &mut self.monitor_set {
            MonitorSet::Normal {
                monitors,
                active_monitor_idx,
                ..
            } => {
                let (mon, insert_ws, position, offset, zoom) =
                    if let Some(mon) = monitors.iter_mut().find(|mon| mon.output == move_.output) {
                        let zoom = mon.overview_zoom();

                        let (insert_ws, geo) = mon.insert_position(move_.pointer_pos_within_output);
                        let (position, offset) = match insert_ws {
                            InsertWorkspace::Existing(ws_id) => {
                                let ws_idx = mon.idx_of_ws(ws_id).unwrap();

                                let position = if move_.is_floating {
                                    InsertPosition::Floating
                                } else {
                                    let pos_within_workspace =
                                        (move_.pointer_pos_within_output - geo.loc).downscale(zoom);
                                    let ws = &mut mon.workspaces[ws_idx];
                                    ws.tiling_insert_position(pos_within_workspace)
                                };

                                (position, Some(geo.loc))
                            }
                            InsertWorkspace::NewAt(_) => {
                                let position = if move_.is_floating {
                                    InsertPosition::Floating
                                } else {
                                    InsertPosition::NewColumn(0)
                                };

                                (position, None)
                            }
                        };

                        (mon, insert_ws, position, offset, zoom)
                    } else {
                        let mon = &mut monitors[*active_monitor_idx];
                        let zoom = mon.overview_zoom();
                        // No point in trying to use the pointer position on the wrong output.
                        let ws = &mon.workspaces[0];
                        let ws_geo = mon.workspaces_render_geo().next().unwrap();

                        let position = if move_.is_floating {
                            InsertPosition::Floating
                        } else {
                            ws.tiling_insert_position(Point::from((0., 0.)))
                        };

                        let insert_ws = InsertWorkspace::Existing(ws.id());
                        (mon, insert_ws, position, Some(ws_geo.loc), zoom)
                    };

                if move_.was_sticky {
                    let tile_render_loc = move_.tile_render_location(zoom);
                    let mut tile = move_.tile;
                    tile.set_sticky(true);
                    tile.floating_pos = None;

                    let ws_idx_for_pos = match insert_ws {
                        InsertWorkspace::Existing(ws_id) => mon
                            .workspaces
                            .iter()
                            .position(|ws| ws.id() == ws_id)
                            .unwrap(),
                        InsertWorkspace::NewAt(_) => mon.active_workspace_idx,
                    };

                    if let (InsertWorkspace::Existing(_), Some(offset)) = (insert_ws, offset) {
                        let pos = (tile_render_loc - offset).downscale(zoom);
                        let pos = mon.workspaces[ws_idx_for_pos].floating_logical_to_size_frac(pos);
                        tile.floating_pos = Some(pos);
                    }

                    mon.add_sticky_tile(tile, true);
                    return;
                }

                let win_id = move_.tile.window().id().clone();
                let tile_render_loc = move_.tile_render_location(zoom);

                let ws_idx = match insert_ws {
                    InsertWorkspace::Existing(ws_id) => mon.idx_of_ws(ws_id).unwrap(),
                    InsertWorkspace::NewAt(ws_idx) => {
                        if mon.options.layout.empty_workspace_above_first && ws_idx == 0 {
                            // Reuse the top empty workspace.
                            0
                        } else if mon.workspaces.len() - 1 <= ws_idx {
                            // Reuse the bottom empty workspace.
                            mon.workspaces.len() - 1
                        } else {
                            mon.add_workspace_at(ws_idx);
                            ws_idx
                        }
                    }
                };

                match position {
                    InsertPosition::NewColumn(column_idx) => {
                        let ws_id = mon.workspaces[ws_idx].id();
                        mon.add_tile(
                            move_.tile,
                            MonitorAddWindowTarget::Workspace {
                                id: ws_id,
                                column_idx: Some(column_idx),
                            },
                            ActivateWindow::Yes,
                            allow_to_activate_workspace,
                            move_.width,
                            false,
                            None,
                        );
                    }
                    InsertPosition::Swap { target, direction } => {
                        // Swapping is only possible back into the workspace the drag started
                        // from, where the vacated slot is still known.
                        let ws_id = mon.workspaces[ws_idx].id();
                        let origin = (move_.origin_workspace == ws_id)
                            .then(|| move_.swap_origin.clone())
                            .flatten();

                        let swap = match origin {
                            Some(origin) => {
                                mon.workspaces[ws_idx].tiling_swap_tile(target, move_.tile, &origin)
                            }
                            None => Err(move_.tile),
                        };

                        match swap {
                            Ok(()) => {
                                if allow_to_activate_workspace {
                                    mon.workspaces[ws_idx].activate_window(&win_id);
                                }
                            }
                            Err(tile) => {
                                let _ = mon.add_tile_split(
                                    ws_idx,
                                    target,
                                    direction,
                                    tile,
                                    true,
                                    allow_to_activate_workspace,
                                );
                            }
                        }
                    }
                    InsertPosition::Split {
                        target, direction, ..
                    } => {
                        let _ = mon.add_tile_split(
                            ws_idx,
                            target,
                            direction,
                            move_.tile,
                            true,
                            allow_to_activate_workspace,
                        );
                    }
                    InsertPosition::SplitRoot { direction, .. } => {
                        let _ = mon.add_tile_split_root(
                            ws_idx,
                            direction,
                            move_.tile,
                            true,
                            allow_to_activate_workspace,
                        );
                    }
                    InsertPosition::Floating => {
                        let mut tile = move_.tile;
                        tile.floating_pos = None;

                        match insert_ws {
                            InsertWorkspace::Existing(_) => {
                                if let Some(offset) = offset {
                                    let pos = (tile_render_loc - offset).downscale(zoom);
                                    let pos =
                                        mon.workspaces[ws_idx].floating_logical_to_size_frac(pos);
                                    tile.floating_pos = Some(pos);
                                } else {
                                    error!(
                                        "offset unset for inserting a floating tile \
                                         to existing workspace"
                                    );
                                }
                            }
                            InsertWorkspace::NewAt(_) => {
                                // When putting a floating tile on a new workspace, we don't really
                                // have a good pre-existing position.
                            }
                        }

                        // Set the floating size so it takes into account any window resizing that
                        // took place during the move.
                        if let Some(size) = tile.window().expected_size() {
                            tile.floating_window_size = Some(size);
                        }

                        let ws_id = mon.workspaces[ws_idx].id();
                        mon.add_tile(
                            tile,
                            MonitorAddWindowTarget::Workspace {
                                id: ws_id,
                                column_idx: None,
                            },
                            ActivateWindow::Yes,
                            allow_to_activate_workspace,
                            move_.width,
                            true,
                            None,
                        );
                    }
                }

                // needed because empty_workspace_above_first could have modified the idx
                if let Some((tile, tile_offset, ws_geo)) = mon
                    .workspaces_with_render_geo_mut(false)
                    .find_map(|(ws, geo)| {
                        ws.tiles_with_render_positions_mut(false)
                            .find(|(tile, _)| tile.window().id() == &win_id)
                            .map(|(tile, tile_offset)| (tile, tile_offset, geo))
                    })
                {
                    let new_tile_render_loc = ws_geo.loc + tile_offset.upscale(zoom);
                    tile.animate_move_from((tile_render_loc - new_tile_render_loc).downscale(zoom));
                }
            }
            MonitorSet::NoOutputs { workspaces, .. } => {
                if workspaces.is_empty() {
                    workspaces.push(Workspace::new_no_outputs(
                        self.clock.clone(),
                        self.options.clone(),
                    ));
                }
                let ws = &mut workspaces[0];

                // No point in trying to use the pointer position without outputs.
                ws.add_tile(
                    move_.tile,
                    WorkspaceAddWindowTarget::Auto,
                    ActivateWindow::Yes,
                    move_.width,
                    move_.is_floating,
                    None,
                );
            }
        }
    }

    pub fn interactive_move_is_moving_above_output(&self, output: &Output) -> bool {
        if let Some(move_) = self
            .interactive_move
            .as_ref()
            .and_then(InteractiveMoveState::moving)
        {
            return move_.output == *output;
        }

        if let Some(move_) = self
            .interactive_move
            .as_ref()
            .and_then(InteractiveMoveState::moving_container)
        {
            return move_.output == *output;
        }

        false
    }

    pub fn dnd_update(&mut self, output: Output, pointer_pos_within_output: Point<f64, Logical>) {
        let begin_gesture = self.dnd.is_none();

        self.dnd = Some(DndData {
            output,
            pointer_pos_within_output,
            hold: None,
        });

        if begin_gesture {
            for mon in self.monitors_mut() {
                mon.dnd_scroll_gesture_begin();
            }
        }
    }

    pub fn dnd_end(&mut self) {
        if self.dnd.is_none() {
            return;
        }

        self.dnd = None;

        for mon in self.monitors_mut() {
            mon.dnd_scroll_gesture_end();
        }
    }

    pub fn interactive_resize_begin(&mut self, window: W::Id, edges: ResizeEdge) -> bool {
        self.request_refresh();
        match &mut self.monitor_set {
            MonitorSet::Normal { monitors, .. } => {
                for mon in monitors.iter_mut() {
                    if mon.has_sticky_window(&window) {
                        return mon.sticky_interactive_resize_begin(window, edges);
                    }
                }
                for mon in monitors {
                    for ws in &mut mon.workspaces {
                        if ws.has_window(&window) {
                            return ws.interactive_resize_begin(window, edges);
                        }
                    }
                }
            }
            MonitorSet::NoOutputs { workspaces, .. } => {
                for ws in workspaces {
                    if ws.has_window(&window) {
                        return ws.interactive_resize_begin(window, edges);
                    }
                }
            }
        }

        false
    }

    pub fn interactive_resize_begin_at(
        &mut self,
        window: W::Id,
        edges: ResizeEdge,
        output: &Output,
        pos_within_output: Point<f64, Logical>,
    ) -> bool {
        self.request_refresh();
        match &mut self.monitor_set {
            MonitorSet::Normal { monitors, .. } => {
                let mon = monitors.iter_mut().find(|mon| &mon.output == output);
                let Some(mon) = mon else {
                    return false;
                };
                if mon.has_sticky_window(&window) {
                    return mon.sticky_interactive_resize_begin(window, edges);
                }
                for (ws, geo) in mon.workspaces_with_render_geo_mut(true) {
                    if ws.has_window(&window) {
                        return ws.interactive_resize_begin_at(
                            window,
                            edges,
                            pos_within_output - geo.loc,
                        );
                    }
                }
            }
            MonitorSet::NoOutputs { workspaces, .. } => {
                for ws in workspaces {
                    if ws.has_window(&window) {
                        return ws.interactive_resize_begin_at(window, edges, pos_within_output);
                    }
                }
            }
        }

        false
    }

    pub fn interactive_resize_update(
        &mut self,
        window: &W::Id,
        delta: Point<f64, Logical>,
    ) -> bool {
        self.request_refresh();
        if let Some(InteractiveMoveState::Moving(move_)) = &self.interactive_move {
            if move_.tile.window().id() == window {
                return false;
            }
        }

        match &mut self.monitor_set {
            MonitorSet::Normal { monitors, .. } => {
                for mon in monitors.iter_mut() {
                    if mon.has_sticky_window(window) {
                        return mon.sticky_interactive_resize_update(window, delta);
                    }
                }
                for mon in monitors {
                    for ws in &mut mon.workspaces {
                        if ws.has_window(window) {
                            return ws.interactive_resize_update(window, delta);
                        }
                    }
                }
            }
            MonitorSet::NoOutputs { workspaces, .. } => {
                for ws in workspaces {
                    if ws.has_window(window) {
                        return ws.interactive_resize_update(window, delta);
                    }
                }
            }
        }

        false
    }

    pub fn interactive_resize_end(&mut self, window: &W::Id) {
        self.request_refresh();
        if let Some(InteractiveMoveState::Moving(move_)) = &self.interactive_move {
            if move_.tile.window().id() == window {
                return;
            }
        }

        match &mut self.monitor_set {
            MonitorSet::Normal { monitors, .. } => {
                for mon in monitors.iter_mut() {
                    if mon.has_sticky_window(window) {
                        mon.sticky_interactive_resize_end(window);
                        return;
                    }
                }
                for mon in monitors {
                    for ws in &mut mon.workspaces {
                        if ws.has_window(window) {
                            ws.interactive_resize_end(Some(window));
                            return;
                        }
                    }
                }
            }
            MonitorSet::NoOutputs { workspaces, .. } => {
                for ws in workspaces {
                    if ws.has_window(window) {
                        ws.interactive_resize_end(Some(window));
                        return;
                    }
                }
            }
        }
    }

    pub fn move_workspace_down(&mut self) {
        let Some(monitor) = self.active_monitor() else {
            return;
        };
        monitor.move_workspace_down();
        self.seat_focus_after_mutation();
    }

    pub fn move_workspace_up(&mut self) {
        let Some(monitor) = self.active_monitor() else {
            return;
        };
        monitor.move_workspace_up();
        self.seat_focus_after_mutation();
    }

    pub fn move_workspace_to_idx(
        &mut self,
        reference: Option<(Option<Output>, usize)>,
        new_idx: usize,
    ) {
        let (monitor, old_idx) = if let Some((output, old_idx)) = reference {
            let monitor = if let Some(output) = output {
                let Some(monitor) = self.monitor_for_output_mut(&output) else {
                    return;
                };
                monitor
            } else {
                // In case a numbered workspace reference is used, assume the active monitor
                let Some(monitor) = self.active_monitor() else {
                    return;
                };
                monitor
            };

            (monitor, old_idx)
        } else {
            let Some(monitor) = self.active_monitor() else {
                return;
            };
            let index = monitor.active_workspace_idx;
            (monitor, index)
        };

        monitor.move_workspace_to_idx(old_idx, new_idx);
        self.seat_focus_after_mutation();
    }

    pub fn move_workspace_to_idx_by_workspace_id(
        &mut self,
        workspace_id: WorkspaceId,
        new_idx: usize,
    ) {
        match &mut self.monitor_set {
            MonitorSet::Normal { monitors, .. } => {
                let Some((mon_idx, old_idx)) =
                    monitors.iter().enumerate().find_map(|(mon_idx, mon)| {
                        mon.workspaces
                            .iter()
                            .position(|ws| ws.id() == workspace_id)
                            .map(|idx| (mon_idx, idx))
                    })
                else {
                    return;
                };

                monitors[mon_idx].move_workspace_to_idx(old_idx, new_idx);
            }
            MonitorSet::NoOutputs { workspaces } => {
                let Some(old_idx) = workspaces.iter().position(|ws| ws.id() == workspace_id) else {
                    return;
                };

                let ws = workspaces.remove(old_idx);
                let new_idx = new_idx.min(workspaces.len());
                workspaces.insert(new_idx, ws);
            }
        }
        self.seat_focus_after_mutation();
    }

    pub fn set_workspace_name(&mut self, name: String, reference: Option<WorkspaceReference>) {
        // ignore the request if the name is already used by another workspace
        if self.find_workspace_by_name(&name).is_some() {
            return;
        }

        let ws = if let Some(reference) = reference {
            self.find_workspace_by_ref(reference)
        } else {
            self.active_workspace_mut()
        };
        let Some(ws) = ws else {
            return;
        };

        ws.set_name(name, WorkspaceLifetime::Persistent);

        let wsid = ws.id();

        // if `empty_workspace_above_first` is set and `ws` is the first
        // workspace on a monitor, another empty workspace needs to
        // be added before.
        // Conversely, if `ws` was the last workspace on a monitor, an
        // empty workspace needs to be added after.

        if let MonitorSet::Normal {
            monitors,
            active_monitor_idx,
            ..
        } = &mut self.monitor_set
        {
            let monitor = &mut monitors[*active_monitor_idx];
            if monitor.options.layout.empty_workspace_above_first
                && monitor
                    .workspaces
                    .first()
                    .is_some_and(|first| first.id() == wsid)
            {
                monitor.add_workspace_top();
            }
            if monitor
                .workspaces
                .last()
                .is_some_and(|last| last.id() == wsid)
            {
                monitor.add_workspace_bottom();
            }
        }
    }

    pub fn unset_workspace_name(&mut self, reference: Option<WorkspaceReference>) {
        let ws = if let Some(reference) = reference {
            self.find_workspace_by_ref(reference)
        } else {
            self.active_workspace_mut()
        };
        let Some(ws) = ws else {
            return;
        };
        let id = ws.id();

        self.unname_workspace_by_id(id);
    }

    pub fn set_monitors_overview_state(&mut self) {
        let MonitorSet::Normal { monitors, .. } = &mut self.monitor_set else {
            return;
        };

        for mon in monitors {
            mon.overview_open = self.overview_open;
            mon.set_overview_progress(self.overview_progress.as_ref());
        }
    }

    pub fn toggle_overview(&mut self) {
        self.overview_open = !self.overview_open;

        let from = self.overview_progress.take().map_or(0., |p| p.value());
        let to = if self.overview_open { 1. } else { 0. };

        self.overview_progress = Some(OverviewProgress::Animation(Animation::new(
            self.clock.clone(),
            from,
            to,
            0.,
            self.options.animations.overview_open_close.0,
        )));

        self.set_monitors_overview_state();
    }

    pub fn open_overview(&mut self) -> bool {
        if self.overview_open {
            return false;
        }

        self.toggle_overview();
        true
    }

    pub fn close_overview(&mut self) -> bool {
        if !self.overview_open {
            return false;
        }

        self.toggle_overview();
        true
    }

    pub fn toggle_overview_to_workspace(&mut self, ws_idx: usize) {
        let config = self.options.animations.overview_open_close.0;
        if let Some(mon) = self.active_monitor() {
            mon.activate_workspace_with_anim_config(ws_idx, Some(config));
        }
        self.toggle_overview();
    }

    pub fn start_open_animation_for_window(&mut self, window: &W::Id) {
        if let Some(InteractiveMoveState::Moving(move_)) = &self.interactive_move {
            if move_.tile.window().id() == window {
                return;
            }
        }

        for ws in self.workspaces_mut() {
            if ws.start_open_animation(window) {
                return;
            }
        }
    }

    pub fn store_unmap_snapshot(
        &mut self,
        renderer: &mut GlesRenderer,
        xray: Option<&mut Xray>,
        xray_has_blocked_out_layers: bool,
        window: &W::Id,
    ) {
        let _span = tracy_client::span!("Layout::store_unmap_snapshot");

        let zoom = self.overview_zoom();

        if let Some(InteractiveMoveState::Moving(move_)) = &mut self.interactive_move {
            if move_.tile.window().id() == window {
                let pos_within_output = move_.tile_render_location(zoom);

                // Computation matches update_render_elements().
                let view_rect =
                    Rectangle::new(pos_within_output.upscale(-1.), output_size(&move_.output))
                        .downscale(zoom);
                move_.tile.update_render_elements(
                    false,
                    false,
                    false,
                    crate::layout::focus_ring::FocusRingEdges::all(),
                    None,
                    view_rect,
                );

                move_.tile.store_unmap_snapshot_if_empty(
                    renderer,
                    xray,
                    xray_has_blocked_out_layers,
                    XrayPos::new(pos_within_output, zoom),
                );
                return;
            }
        }

        match &mut self.monitor_set {
            MonitorSet::Normal { monitors, .. } => {
                for mon in monitors {
                    for (ws, geo) in mon.workspaces_with_render_geo_mut(false) {
                        if ws.has_window(window) {
                            ws.store_unmap_snapshot_if_empty(
                                renderer,
                                xray,
                                xray_has_blocked_out_layers,
                                XrayPos::new(geo.loc, zoom),
                                window,
                            );
                            return;
                        }
                    }
                }
            }
            MonitorSet::NoOutputs { workspaces, .. } => {
                for ws in workspaces {
                    if ws.has_window(window) {
                        ws.store_unmap_snapshot_if_empty(
                            renderer,
                            xray,
                            xray_has_blocked_out_layers,
                            XrayPos::default(),
                            window,
                        );
                        return;
                    }
                }
            }
        }
    }

    pub fn clear_unmap_snapshot(&mut self, window: &W::Id) {
        if let Some(InteractiveMoveState::Moving(move_)) = &mut self.interactive_move {
            if move_.tile.window().id() == window {
                let _ = move_.tile.take_unmap_snapshot();
                return;
            }
        }

        match &mut self.monitor_set {
            MonitorSet::Normal { monitors, .. } => {
                for mon in monitors {
                    for ws in &mut mon.workspaces {
                        if ws.has_window(window) {
                            ws.clear_unmap_snapshot(window);
                            return;
                        }
                    }
                }
            }
            MonitorSet::NoOutputs { workspaces, .. } => {
                for ws in workspaces {
                    if ws.has_window(window) {
                        ws.clear_unmap_snapshot(window);
                        return;
                    }
                }
            }
        }
    }

    pub fn start_close_animation_for_window(
        &mut self,
        renderer: &mut GlesRenderer,
        window: &W::Id,
        blocker: TransactionBlocker,
    ) {
        let _span = tracy_client::span!("Layout::start_close_animation_for_window");

        let zoom = self.overview_zoom();

        if let Some(InteractiveMoveState::Moving(move_)) = &mut self.interactive_move {
            if move_.tile.window().id() == window {
                let Some(snapshot) = move_.tile.take_unmap_snapshot() else {
                    return;
                };
                let tile_pos = move_.tile_render_location(zoom);
                let tile_size = move_.tile.tile_size();

                let output = move_.output.clone();
                let pointer_pos_within_output = move_.pointer_pos_within_output;
                let Some(mon) = self.monitor_for_output_mut(&output) else {
                    return;
                };
                let Some((ws, ws_geo)) = mon.workspace_under(pointer_pos_within_output) else {
                    return;
                };
                let idx = mon.idx_of_ws(ws.id()).unwrap();
                let ws = &mut mon.workspaces[idx];

                let tile_pos = tile_pos - ws_geo.loc;
                ws.start_close_animation_for_tile(renderer, snapshot, tile_size, tile_pos, blocker);
                return;
            }
        }

        match &mut self.monitor_set {
            MonitorSet::Normal { monitors, .. } => {
                for mon in monitors {
                    // Check sticky windows first.
                    if mon.has_sticky_window(window) {
                        mon.start_close_animation_for_sticky_window(renderer, window, blocker);
                        return;
                    }

                    for ws in &mut mon.workspaces {
                        if ws.has_window(window) {
                            ws.start_close_animation_for_window(renderer, window, blocker);
                            return;
                        }
                    }
                }
            }
            MonitorSet::NoOutputs { workspaces, .. } => {
                for ws in workspaces {
                    if ws.has_window(window) {
                        ws.start_close_animation_for_window(renderer, window, blocker);
                        return;
                    }
                }
            }
        }
    }

    pub fn render_interactive_move_for_output<R: NiriRenderer>(
        &self,
        ctx: RenderCtx<R>,
        output: &Output,
        push: &mut dyn FnMut(RescaleRenderElement<TileRenderElement<R>>),
    ) {
        if self.update_render_elements_time != self.clock.now() {
            error!("clock moved between updating render elements and rendering");
        }

        let Some(InteractiveMoveState::Moving(move_)) = &self.interactive_move else {
            return;
        };

        if &move_.output != output {
            return;
        }

        let scale = Scale::from(move_.output.current_scale().fractional_scale());
        let zoom = self.overview_zoom();
        let pos_in_backdrop = move_.tile_render_location(zoom);
        let xray_pos = XrayPos::new(pos_in_backdrop, zoom);

        move_
            .tile
            .render(ctx, pos_in_backdrop, xray_pos, true, &mut |elem| {
                push(RescaleRenderElement::from_element(
                    elem,
                    pos_in_backdrop.to_physical_precise_round(scale),
                    zoom,
                ));
            });
    }

    pub fn refresh(&mut self, is_active: bool) {
        let _span = tracy_client::span!("Layout::refresh");

        self.is_active = is_active;

        let mut ongoing_scrolling_dnd = self.dnd.is_some().then_some(true);

        if let Some(InteractiveMoveState::Moving(move_)) = &mut self.interactive_move {
            let win = move_.tile.window_mut();

            win.set_active_in_column(true);
            win.set_floating(move_.is_floating);
            win.set_activated(true);

            win.set_interactive_resize(None);

            win.set_bounds(output_size(&move_.output).to_i32_round());

            win.send_pending_configure();
            win.refresh();

            ongoing_scrolling_dnd.get_or_insert(!move_.is_floating);
        } else if let Some(InteractiveMoveState::Starting { window_id, .. }) =
            &self.interactive_move
        {
            ongoing_scrolling_dnd.get_or_insert_with(|| {
                self.workspaces()
                    .find(|(_, _, ws)| ws.has_window(window_id))
                    .is_some_and(|(_, _, ws)| !ws.is_floating(window_id))
            });
        }

        match &mut self.monitor_set {
            MonitorSet::Normal {
                monitors,
                active_monitor_idx,
                ..
            } => {
                for (idx, mon) in monitors.iter_mut().enumerate() {
                    let is_active = self.is_active
                        && idx == *active_monitor_idx
                        && !self
                            .interactive_move
                            .as_ref()
                            .is_some_and(InteractiveMoveState::is_moving);

                    if ongoing_scrolling_dnd.is_some() && self.overview_open {
                        // Begin the scroll on new monitors and when opening the overview.
                        mon.dnd_scroll_gesture_begin();
                    } else if !self.overview_open {
                        mon.dnd_scroll_gesture_end();
                    }

                    for (ws_idx, ws) in mon.workspaces.iter_mut().enumerate() {
                        let is_focused = is_active && ws_idx == mon.active_workspace_idx;
                        ws.refresh(is_active, is_focused);

                        if ongoing_scrolling_dnd.is_none() {
                            // Cancel the horizontal view gesture after workspace switches, moves, etc.
                            if !self.overview_open && ws_idx != mon.active_workspace_idx {
                                ws.horizontal_view_gesture_end(None);
                            }
                        }
                    }

                    let sticky_active = is_active && mon.sticky_is_active();
                    mon.refresh_sticky(sticky_active);
                }
            }
            MonitorSet::NoOutputs { workspaces, .. } => {
                for ws in workspaces {
                    ws.refresh(false, false);
                    ws.horizontal_view_gesture_end(None);
                }
            }
        }

        // The scratchpad is a workspace and is refreshed like one. This is what keeps a
        // hidden window in step with its client: its configures go out and are acked while it
        // is away, so bringing it back is a move rather than a negotiation.
        self.scratchpad.refresh(false, false);
    }

    pub fn are_window_resize_animations_enabled(&self) -> bool {
        !self.options.animations.off && !self.options.animations.window_resize.anim.off
    }

    pub fn workspaces(
        &self,
    ) -> impl Iterator<Item = (Option<&Monitor<W>>, usize, &Workspace<W>)> + '_ {
        let iter_normal;
        let iter_no_outputs;

        match &self.monitor_set {
            MonitorSet::Normal { monitors, .. } => {
                let it = monitors.iter().flat_map(|mon| {
                    mon.workspaces
                        .iter()
                        .enumerate()
                        .map(move |(idx, ws)| (Some(mon), idx, ws))
                });

                iter_normal = Some(it);
                iter_no_outputs = None;
            }
            MonitorSet::NoOutputs { workspaces } => {
                let it = workspaces
                    .iter()
                    .enumerate()
                    .map(|(idx, ws)| (None, idx, ws));

                iter_normal = None;
                iter_no_outputs = Some(it);
            }
        }

        let iter_normal = iter_normal.into_iter().flatten();
        let iter_no_outputs = iter_no_outputs.into_iter().flatten();
        iter_normal.chain(iter_no_outputs)
    }

    pub fn workspaces_mut(&mut self) -> impl Iterator<Item = &mut Workspace<W>> + '_ {
        let iter_normal;
        let iter_no_outputs;

        match &mut self.monitor_set {
            MonitorSet::Normal { monitors, .. } => {
                let it = monitors
                    .iter_mut()
                    .flat_map(|mon| mon.workspaces.iter_mut());

                iter_normal = Some(it);
                iter_no_outputs = None;
            }
            MonitorSet::NoOutputs { workspaces } => {
                let it = workspaces.iter_mut();

                iter_normal = None;
                iter_no_outputs = Some(it);
            }
        }

        let iter_normal = iter_normal.into_iter().flatten();
        let iter_no_outputs = iter_no_outputs.into_iter().flatten();
        iter_normal.chain(iter_no_outputs)
    }

    pub fn windows(&self) -> impl Iterator<Item = (Option<&Monitor<W>>, &W)> {
        let moving_window = self
            .interactive_move
            .as_ref()
            .and_then(|x| x.moving())
            .map(|move_| (self.monitor_for_output(&move_.output), move_.tile.window()))
            .into_iter();

        let rest = self
            .workspaces()
            .flat_map(|(mon, _, ws)| ws.windows().map(move |win| (mon, win)));

        let sticky = self
            .monitors()
            .flat_map(|mon| mon.sticky_windows().map(move |win| (Some(mon), win)));

        let scratchpad = self.scratchpad.tiles().map(|tile| (None, tile.window()));

        moving_window.chain(rest).chain(sticky).chain(scratchpad)
    }

    fn active_mark_target_key(&self) -> Option<NodeKey> {
        self.active_monitor_ref()?.active_mark_target_key()
    }

    fn node_has_mark(&self, key: NodeKey, mark: &str) -> bool {
        self.scratchpad.node_has_mark(key, mark)
            || self
                .monitors()
                .any(|monitor| monitor.sticky_node_has_mark(key, mark))
            || self
                .workspaces()
                .any(|(_, _, workspace)| workspace.node_has_mark(key, mark))
    }

    fn add_mark_to_node(&mut self, key: NodeKey, mark: String) {
        if self.scratchpad.holds_node(key) {
            let _ = self.scratchpad.add_mark_to_node(key, mark);
            return;
        }

        if let Some(monitor) = self
            .monitors_mut()
            .find(|monitor| monitor.sticky_holds_node(key))
        {
            let _ = monitor.add_mark_to_sticky_node(key, mark);
            return;
        }

        if let Some(workspace) = self
            .workspaces_mut()
            .find(|workspace| workspace.holds_node(key))
        {
            let _ = workspace.add_mark_to_node(key, mark);
        }
    }

    fn remove_mark_from_node(&mut self, key: NodeKey, mark: &str) {
        if self.scratchpad.holds_node(key) {
            let _ = self.scratchpad.remove_mark_from_node(key, mark);
            return;
        }

        if let Some(monitor) = self
            .monitors_mut()
            .find(|monitor| monitor.sticky_holds_node(key))
        {
            let _ = monitor.remove_mark_from_sticky_node(key, mark);
            return;
        }

        if let Some(workspace) = self
            .workspaces_mut()
            .find(|workspace| workspace.holds_node(key))
        {
            let _ = workspace.remove_mark_from_node(key, mark);
        }
    }

    fn clear_marks_on_node(&mut self, key: NodeKey) {
        if self.scratchpad.holds_node(key) {
            let _ = self.scratchpad.clear_marks_on_node(key);
            return;
        }

        if let Some(monitor) = self
            .monitors_mut()
            .find(|monitor| monitor.sticky_holds_node(key))
        {
            let _ = monitor.clear_marks_on_sticky_node(key);
            return;
        }

        if let Some(workspace) = self
            .workspaces_mut()
            .find(|workspace| workspace.holds_node(key))
        {
            let _ = workspace.clear_marks_on_node(key);
        }
    }

    fn clear_marks_everywhere(&mut self) {
        if let Some(InteractiveMoveState::Moving(move_)) = &mut self.interactive_move {
            move_.tile.clear_marks();
        }

        self.scratchpad.clear_marks_everywhere();
        for mon in self.monitors_mut() {
            mon.clear_sticky_marks();
        }
        for ws in self.workspaces_mut() {
            ws.clear_marks_everywhere();
        }
    }

    fn remove_mark_everywhere(&mut self, mark: &str) {
        if let Some(InteractiveMoveState::Moving(move_)) = &mut self.interactive_move {
            move_.tile.remove_mark(mark);
        }

        self.scratchpad.remove_mark_everywhere(mark);
        for mon in self.monitors_mut() {
            mon.remove_mark_from_sticky(mark);
        }
        for ws in self.workspaces_mut() {
            ws.remove_mark_everywhere(mark);
        }
    }

    pub fn has_window(&self, window: &W::Id) -> bool {
        self.windows().any(|(_, win)| win.id() == window)
    }

    #[cfg(test)]
    pub fn scratchpad_for_test(&self) -> &Workspace<W> {
        &self.scratchpad
    }

    pub fn is_overview_open(&self) -> bool {
        self.overview_open
    }
}

impl<W: LayoutElement> Layout<W> {
    pub fn layout_tree(&self) -> LayoutTree {
        let Some(monitor) = self.active_monitor_ref() else {
            return LayoutTree {
                workspace_id: None,
                workspace_name: None,
                output: None,
                root: None,
                floating: Vec::new(),
            };
        };
        let workspace = &monitor.workspaces[monitor.active_workspace_idx];

        LayoutTree {
            workspace_id: Some(workspace.id().get()),
            workspace_name: workspace.name().cloned(),
            output: Some(monitor.output.name()),
            root: workspace.layout_tree(),
            floating: workspace.floating_layout_tree_nodes(),
        }
    }
}

impl<W: LayoutElement> Default for MonitorSet<W> {
    fn default() -> Self {
        Self::NoOutputs { workspaces: vec![] }
    }
}

fn compute_overview_zoom(options: &Options, overview_progress: Option<f64>) -> f64 {
    // Clamp to some sane values.
    let zoom = options.overview.zoom.clamp(0.0001, 0.75);

    if let Some(p) = overview_progress {
        (1. - p * (1. - zoom)).max(0.0001)
    } else {
        1.
    }
}
