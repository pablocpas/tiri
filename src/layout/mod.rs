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
//! Therefore, we implement the following logic: every workspace keeps track of its ordered
//! original output candidates. When an output disconnects, its workspaces move to the first
//! connected candidate or the (potentially new) primary output. When a higher-priority candidate
//! connects, matching workspaces move to it.
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

use container_tree::{ContainerTree, RootTilingSubtree};
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
use tiri_ipc::{LayoutTree, PositionChange, SizeChange, WindowLayout};
use workspace::{WorkspaceAddWindowTarget, WorkspaceId, WorkspaceLifetime};

pub use self::container::LayoutCommand;
pub use self::container::{Direction, LayoutMode};
use self::container::{IncomingView, InsertParentInfo, NodeKey, OutputKey};
pub use self::interactive_move::DndData;
use self::interactive_move::{DndHold, DndHoldTarget, InteractiveMoveState};
pub use self::monitor::MonitorRenderElement;
use self::monitor::{Monitor, WorkspaceSwitch};
use self::workspace::Workspace;
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
pub enum LayoutCycleEntry {
    /// Match either split orientation and switch orientation when this entry is selected.
    Split,
    /// Match and select one concrete layout.
    Layout(LayoutMode),
}

pub mod closing_window;
pub mod container;
pub mod container_tree;
pub mod floating;
pub mod focus_ring;
pub mod insert_hint_element;
mod interactive_move;
pub mod monitor;
pub mod opening_window;
pub mod shadow;
pub mod tab_bar;
pub mod tile;
pub mod workspace;

#[cfg(test)]
mod tests;

/// Size changes up to this many pixels don't animate.
pub const RESIZE_ANIMATION_THRESHOLD: f64 = 10.;

/// A focus command within the active workspace.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FocusCommand {
    /// sway's `focus left|right|up|down`.
    Direction(Direction),
    /// The window furthest towards the direction: niri's first and last column, top and
    /// bottom window.
    Edge(Direction),
    /// Focus in the direction, or wrap round to the window at the opposite edge.
    DirectionOrEdge(Direction),
    /// Focus in the first direction, or failing that the second.
    DirectionOr(Direction, Direction),
    /// sway's `focus parent`.
    Parent,
    /// sway's `focus child`.
    Child,
    /// sway's `focus next|prev [sibling]`: along the parent's orientation, `forward` for
    /// `next`; `descend` unless `sibling`.
    Sibling { forward: bool, descend: bool },
    /// sway's `focus floating`.
    Floating,
    /// sway's `focus tiling`.
    Tiling,
    /// sway's `focus mode_toggle`.
    ModeToggle,
    /// niri's `focus-column <index>`: the root container at this 1-based index.
    RootContainer(usize),
    /// niri's `focus-window-in-column <index>`: the 1-based window of the focused root
    /// container.
    WindowInRootContainer(u8),
}

/// A size command, aimed at a window or, without one, at what the active workspace's
/// commands are aimed at.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum SizeCommand {
    /// sway's `resize set|grow|shrink`, and niri's `set-window-width|height`.
    Resize(ResizeRequest),
    /// niri's `switch-preset-window-width|height`: the next or previous preset size.
    CyclePreset { axis: ResizeAxis, forwards: bool },
    /// niri's `reset-window-height`.
    ResetHeight,
    /// niri's `switch-preset-column-width`, on the root container holding the focus.
    CycleRootPreset { forwards: bool },
    /// niri's `set-column-width`, on the root container holding the focus.
    SetRootWidth(SizeChange),
    /// niri's `expand-column-to-available-width`, on the root container holding the focus.
    ExpandRootToAvailableWidth,
}

/// A position among a workspace's root containers: niri's columns.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RootPosition {
    First,
    Last,
    /// 1-based, as user-facing commands count.
    Index(usize),
}

/// Which workspace an output is to show.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WorkspaceTarget {
    /// The workspace above the one shown.
    Up,
    /// The workspace below the one shown.
    Down,
    /// The workspace at this index.
    Index(usize),
    /// The workspace at this index, or the previous one when it is already shown: i3's
    /// `workspace_auto_back_and_forth`.
    IndexOrBack(usize),
    /// The workspace shown before the current one.
    Previous,
}

/// What a transfer to another workspace or output moves.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TransferSubject<'a, Id> {
    /// One window: niri's `move-window-to-*`; `None` for the focused one.
    Window(Option<&'a Id>),
    /// i3's `move container to …`: whatever the seat has selected, leaf or container.
    Container,
    /// niri's `move-column-to-*`: the root container holding the focus.
    RootContainer,
}

/// Where a transfer goes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TransferTarget<'a> {
    /// The workspace above on the subject's output.
    WorkspaceUp,
    /// The workspace below on the subject's output.
    WorkspaceDown,
    /// A workspace of the subject's output, by index.
    WorkspaceIndex(usize),
    /// A workspace wherever it is, by id; crossing outputs when it is on another one.
    Workspace(WorkspaceId),
    /// An output: onto the workspace it shows, or the one at `workspace` on it.
    Output {
        output: &'a Output,
        workspace: Option<usize>,
    },
}

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
    type Id: Eq + std::hash::Hash + std::fmt::Debug + Clone;

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
    fn interactive_resize_data(&self) -> Option<InteractiveResizeData>;

    fn on_commit(&mut self, serial: Serial);
}

#[derive(Debug)]
pub struct Layout<W: LayoutElement> {
    /// The single container tree shared by every workspace, sticky window, scratchpad
    /// entry, and interactive move transfer in this layout.
    containers: ContainerTree<W>,
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
    /// Whether the layout holds keyboard focus, as opposed to a layer-shell surface or a lock.
    ///
    /// While it does not, workspace switches are not recorded in the seat's order: nothing in
    /// the layout was focused.
    has_layout_focus: bool,
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
    /// for a window to be in. Being a workspace is also what keeps a hidden window arranged
    /// and its client in step, so showing it is a move rather than a resize handshake with
    /// an idle client.
    scratchpad: Workspace<W>,
    /// Where the workspaces are while no output is connected: sway's `root->fallback_output`.
    ///
    /// An output of the tree like any other, so that which workspaces exist and in what order
    /// is the tree's to say whether or not an output is connected.
    fallback_output: OutputKey,
    /// Configurable properties of the layout.
    options: Rc<Options>,
    /// A semantic layout mutation happened since the compositor last refreshed projections.
    ///
    /// Niri also classifies its public action/protocol entry points, but this bit is the
    /// authoritative signal for callers that intentionally operate on Layout directly.
    refresh_requested: bool,
}

#[derive(Debug, Default)]
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
    /// No outputs are connected; the workspaces are on [`Layout::fallback_output`].
    #[default]
    NoOutputs,
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

/// Existing view in transit between two roots of the layout-wide tree.
#[derive(Debug)]
#[must_use = "a view transfer must be reattached or finally removed"]
struct ViewTransfer {
    node: NodeKey,
    /// Whether the view left a floating branch.
    is_floating: bool,
}

impl ViewTransfer {
    fn tile<'a, W: LayoutElement>(&self, containers: &'a ContainerTree<W>) -> &'a Tile<W> {
        containers
            .transferred_tile(self.node)
            .expect("a view transfer must remain in the tree")
    }

    fn tile_mut<'a, W: LayoutElement>(
        &self,
        containers: &'a mut ContainerTree<W>,
    ) -> &'a mut Tile<W> {
        containers
            .transferred_tile_mut(self.node)
            .expect("a view transfer must remain in the tree")
    }

    fn prepare_for_workspace_move<W: LayoutElement>(&self, containers: &mut ContainerTree<W>) {
        if !self.is_floating {
            containers.unset_node_fractions(self.node);
        }
    }

    fn into_incoming<W: LayoutElement>(self) -> IncomingView<W> {
        IncomingView::Transferred(self.node)
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

/// Presentation inputs shared by the two branches of a workspace for one render pass.
#[derive(Clone, Copy)]
struct BranchRenderParams {
    has_focus: bool,
    focus_ring: bool,
    layer: RenderLayer,
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

impl ActivateWindow {
    /// `Smart` when the caller asked for focus to follow the window, `No` otherwise.
    pub fn following(focus: bool) -> Self {
        if focus {
            Self::Smart
        } else {
            Self::No
        }
    }

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
        let mut containers = ContainerTree::new(clock.clone());
        let scratchpad = Workspace::new_scratchpad(&mut containers, options.clone());
        let fallback_output = containers.add_output(None);
        Self {
            containers,
            monitor_set: MonitorSet::NoOutputs,
            fallback_output,
            is_active: true,
            last_active_workspace_id: HashMap::new(),
            has_layout_focus: true,
            interactive_move: None,
            dnd: None,
            scratchpad,
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

        let mut containers = ContainerTree::new(clock.clone());
        let scratchpad = Workspace::new_scratchpad(&mut containers, opts.clone());

        let fallback_output = containers.add_output(None);
        for (idx, ws) in config.workspaces.iter().enumerate() {
            Workspace::new_with_config_no_outputs(&mut containers, Some(ws.clone()), opts.clone())
                .attach(&mut containers, fallback_output, idx);
        }

        Self {
            containers,
            monitor_set: MonitorSet::NoOutputs,
            fallback_output,
            is_active: true,
            last_active_workspace_id: HashMap::new(),
            has_layout_focus: true,
            interactive_move: None,
            dnd: None,
            scratchpad,
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
                // Every workspace that prefers the new output over the one it is on goes to it,
                // whichever output that is: with fallbacks, a workspace may be on its second
                // choice, not only on the primary.
                let new_output_idx = monitors.len();
                let mut workspaces = vec![];
                for source_idx in 0..monitors.len() {
                    let mut stopped_ws_switch = false;

                    for i in (0..monitors[source_idx].workspace_count(&self.containers)).rev() {
                        let preferred = monitors[source_idx]
                            .ws(&self.containers, i)
                            .find_preferred_output(
                                &self.containers,
                                monitors
                                    .iter()
                                    .map(|monitor| &monitor.output)
                                    .chain(std::iter::once(&output)),
                            )
                            == Some(new_output_idx);
                        if !preferred {
                            continue;
                        }

                        let source = &mut monitors[source_idx];
                        let ws = source.release_workspace_at(&mut self.containers, i);

                        // FIXME: this can be coded in a way that the workspace switch won't be
                        // affected if the removed workspace is invisible. But this is good enough
                        // for now.
                        if source.workspace_switch.is_some() {
                            source.workspace_switch = None;
                            stopped_ws_switch = true;
                        }

                        // The user could've closed a window while remaining on this workspace, on
                        // another monitor. However, we will add an empty workspace in the end
                        // instead.
                        if ws.has_windows_or_persistent_identity(&self.containers) {
                            workspaces.push(ws);
                        } else {
                            ws.remove_from_tree(&mut self.containers);
                        }
                    }

                    let source = &mut monitors[source_idx];

                    // A workspace going back to its output may have been the one this output
                    // showed; it takes its sticky floats along, and whichever is shown now holds
                    // this output's from here on.
                    source.sync_sticky_host(&mut self.containers);

                    // If we stopped a workspace switch, then we might need to clean up workspaces.
                    // Also if empty_workspace_above_first is set and there are only 2 workspaces
                    // left, both will be empty and one of them needs to be removed.
                    // clean_up_workspaces takes care of this.
                    if stopped_ws_switch
                        || (source.options.layout.empty_workspace_above_first
                            && source.workspace_count(&self.containers) == 2)
                    {
                        source.clean_up_workspaces(&mut self.containers);
                    }
                }
                workspaces.reverse();

                let ws_id_to_activate = self.last_active_workspace_id.remove(&output.name());

                let mut monitor = Monitor::new(
                    &mut self.containers,
                    output,
                    workspaces,
                    ws_id_to_activate,
                    self.clock.clone(),
                    self.options.clone(),
                    layout_config,
                );
                monitor.overview_open = self.overview_open;
                monitor.set_overview_progress(&self.containers, self.overview_progress.as_ref());
                monitors.push(monitor);

                MonitorSet::Normal {
                    monitors,
                    primary_idx,
                    active_monitor_idx,
                }
            }
            MonitorSet::NoOutputs => {
                let workspaces = self.outputless_workspaces();
                for ws in &workspaces {
                    ws.detach(&mut self.containers);
                }
                let seed_initial_workspace = workspaces.is_empty();
                let ws_id_to_activate = self.last_active_workspace_id.remove(&output.name());

                let mut monitor = Monitor::new(
                    &mut self.containers,
                    output,
                    workspaces,
                    ws_id_to_activate,
                    self.clock.clone(),
                    self.options.clone(),
                    layout_config,
                );
                if seed_initial_workspace {
                    monitor.set_initial_numeric_workspace(
                        &mut self.containers,
                        1,
                        WorkspaceLifetime::Transient,
                    );
                }
                monitor.overview_open = self.overview_open;
                monitor.set_overview_progress(&self.containers, self.overview_progress.as_ref());

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
                let mut monitor = monitors.remove(idx);

                self.last_active_workspace_id.insert(
                    monitor.output_name().clone(),
                    monitor
                        .ws(
                            &self.containers,
                            monitor.active_workspace_idx(&self.containers),
                        )
                        .id(),
                );

                if primary_idx >= idx {
                    // Update primary_idx to either still point at the same monitor, or at some
                    // other monitor if the primary has been removed.
                    primary_idx = primary_idx.saturating_sub(1);
                }

                // The removed output's workspaces go to the outputs they prefer next, or to the
                // primary one. Those that hold nothing but sticky floats do not survive the
                // move, so the floats go to the workspace the primary output is showing
                // instead, as sway's do.
                if let Some(primary) = monitors.get_mut(primary_idx) {
                    let destination = primary.active_workspace(&self.containers);
                    monitor.evacuate_sticky_floats(&mut self.containers, destination);
                    primary.keep_end_workspaces_empty(&mut self.containers);
                }

                let workspaces = monitor.into_workspaces(&mut self.containers);

                if monitors.is_empty() {
                    // Removed the last monitor.

                    for (idx, ws) in workspaces.iter().enumerate() {
                        ws.attach(&mut self.containers, self.fallback_output, idx);
                        // Reset base options to layout ones.
                        ws.update_config(&mut self.containers, self.options.clone());
                    }

                    MonitorSet::NoOutputs
                } else {
                    if active_monitor_idx >= idx {
                        // Update active_monitor_idx to either still point at the same monitor, or
                        // at some other monitor if the active monitor has
                        // been removed.
                        active_monitor_idx = active_monitor_idx.saturating_sub(1);
                    }

                    // Each workspace goes to the connected output it prefers, or to the primary
                    // one if it prefers none of them.
                    let mut workspaces = workspaces;
                    for target_idx in 0..monitors.len() {
                        let (target_workspaces, rest) =
                            workspaces.into_iter().partition::<Vec<_>, _>(|ws| {
                                ws.find_preferred_output(
                                    &self.containers,
                                    monitors.iter().map(|monitor| &monitor.output),
                                )
                                .unwrap_or(primary_idx)
                                    == target_idx
                            });
                        workspaces = rest;
                        monitors[target_idx]
                            .append_workspaces(&mut self.containers, target_workspaces);
                    }

                    MonitorSet::Normal {
                        monitors,
                        primary_idx,
                        active_monitor_idx,
                    }
                }
            }
            MonitorSet::NoOutputs => {
                panic!("tried to remove output when there were already none")
            }
        };
        self.seat_focus_after_mutation();
    }

    pub fn add_root_tiling_subtree_by_idx(
        &mut self,
        monitor_idx: usize,
        workspace_idx: usize,
        subtree: RootTilingSubtree,
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

        monitors[monitor_idx].add_root_tiling_subtree(
            &mut self.containers,
            workspace_idx,
            subtree,
            activate,
        );

        if activate {
            *active_monitor_idx = monitor_idx;
        }
    }

    /// Adds a new window to the layout.
    ///
    /// Returns an output that the window was added to, if there were any outputs.
    #[allow(clippy::too_many_arguments)]
    pub fn add_window(
        &mut self,
        window: W,
        target: AddWindowTarget<W>,
        height: Option<PresetSize>,
        is_floating: bool,
        activate: ActivateWindow,
    ) -> Option<Output> {
        let tiling_height = height.map(SizeChange::from);
        let id = window.id().clone();
        let next_to_workspace = match &target {
            AddWindowTarget::NextTo(next_to) => self.workspace_for_window(next_to),
            _ => None,
        };
        let next_to_monitor = next_to_workspace
            .and_then(|ws| self.monitor_location(ws.workspace_key()))
            .map(|(monitor_idx, _)| monitor_idx);

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
                        let mon_idx = monitors
                            .iter()
                            .position(|mon| mon.has_ws(&self.containers, ws_id))
                            .unwrap();

                        (
                            mon_idx,
                            MonitorAddWindowTarget::Workspace {
                                id: ws_id,
                                root_index: None,
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
                            .filter(|move_| next_to == &move_.window_id)
                            .map(|move_| move_.output.clone())
                        {
                            // The next_to window is being interactively moved.
                            let mon_idx = monitors
                                .iter()
                                .position(|mon| mon.output == output)
                                .unwrap_or(*active_monitor_idx);

                            (mon_idx, MonitorAddWindowTarget::Auto)
                        } else {
                            let mon_idx = next_to_monitor
                                .expect("next_to must name a window on a workspace of an output");
                            (mon_idx, MonitorAddWindowTarget::NextTo(next_to))
                        }
                    }
                };
                let mon = &mut monitors[mon_idx];

                mon.add_window(&mut self.containers, window, target, activate, is_floating);

                if activate.map_smart(|| false) {
                    *active_monitor_idx = mon_idx;
                }

                Some(mon.output.clone())
            }
            MonitorSet::NoOutputs => {
                let (ws, target) = match target {
                    AddWindowTarget::Auto => (
                        self.first_outputless_workspace(),
                        WorkspaceAddWindowTarget::Auto,
                    ),
                    AddWindowTarget::Output(_) => {
                        panic!("cannot target an output for a new window when none are connected")
                    }
                    AddWindowTarget::Workspace(ws_id) => {
                        let ws = self
                            .outputless_workspaces()
                            .into_iter()
                            .find(|ws| ws.id() == ws_id)
                            .unwrap();
                        (ws, WorkspaceAddWindowTarget::Auto)
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
                            .filter(|move_| next_to == &move_.window_id)
                            .is_some()
                        {
                            // The next_to window is being interactively moved. If there are no
                            // other windows, we may have no workspaces at all.
                            (
                                self.first_outputless_workspace(),
                                WorkspaceAddWindowTarget::Auto,
                            )
                        } else {
                            let ws = next_to_workspace
                                .expect("next_to must name a window on a workspace");
                            (ws, WorkspaceAddWindowTarget::NextTo(next_to))
                        }
                    }
                };

                let tile = ws.make_tile(&self.containers, window);
                ws.add_tile(
                    &mut self.containers,
                    tile.into(),
                    target,
                    activate,
                    is_floating,
                );

                None
            }
        };

        // Set the default height for tiling windows.
        if !is_floating {
            if let Some(change) = tiling_height {
                let ws = self
                    .workspace_for_window(&id)
                    .expect("the window was just added to a workspace");
                ws.size_command(
                    &mut self.containers,
                    Some(&id),
                    SizeCommand::Resize(ResizeRequest::Axis {
                        axis: ResizeAxis::Vertical,
                        change,
                    }),
                );
            }
        }

        self.seat_focus_after_mutation();
        output
    }

    pub fn remove_window(&mut self, window: &W::Id, transaction: Transaction) -> bool {
        let removed = self.remove_window_inner(window, transaction);
        if removed {
            self.seat_focus_after_mutation();
        }
        removed
    }

    fn remove_window_inner(&mut self, window: &W::Id, transaction: Transaction) -> bool {
        if let Some(state) = &self.interactive_move {
            match state {
                InteractiveMoveState::Starting { window_id, .. } => {
                    if window_id == window {
                        self.interactive_move_end(window);
                    }
                }
                InteractiveMoveState::Moving(move_) => {
                    if &move_.window_id == window {
                        let Some(InteractiveMoveState::Moving(move_)) =
                            self.interactive_move.take()
                        else {
                            unreachable!()
                        };

                        self.end_dnd_scroll_gestures();

                        assert!(self.containers.destroy_transferred_subtree(move_.node));
                        return true;
                    }
                }
                InteractiveMoveState::MovingContainer(move_) => {
                    if &move_.window_id == window {
                        self.interactive_move_end(window);
                    }
                }
            }
        }

        let Some(transferred) = self.take_window_for_transfer(window, transaction) else {
            return false;
        };
        assert!(self
            .containers
            .destroy_transferred_subtree(transferred.node));
        true
    }

    pub fn descendants_added(&mut self, id: &W::Id) -> bool {
        self.workspace_handles()
            .into_iter()
            .any(|ws| ws.descendants_added(&mut self.containers, id))
    }

    pub fn update_window(&mut self, window: &W::Id, serial: Option<Serial>) {
        if let Some(InteractiveMoveState::Moving(move_)) = &mut self.interactive_move {
            if &move_.window_id == window {
                let tile = self
                    .containers
                    .transferred_tile_mut(move_.node)
                    .expect("moving view must remain transferred");
                // Do this before calling update_window() so it can get up-to-date info.
                if let Some(serial) = serial {
                    tile.window_mut().on_commit(serial);
                }

                tile.update_window();
                return;
            }
        }

        let tree = &self.containers;
        let Some(workspace) = tree
            .window_key(window)
            .and_then(|key| tree.workspace_of(key))
        else {
            return;
        };
        Workspace::of(workspace).update_window(&mut self.containers, window, serial);
    }

    /// The workspace matching `predicate`, with its index on its output or among those waiting
    /// for one.
    fn find_workspace(
        &self,
        mut predicate: impl FnMut(Workspace<W>) -> bool,
    ) -> Option<(usize, Workspace<W>)> {
        self.workspaces()
            .find(|&(_, _, ws)| predicate(ws))
            .map(|(_, idx, ws)| (idx, ws))
    }

    pub fn find_workspace_by_id(&self, id: WorkspaceId) -> Option<(usize, Workspace<W>)> {
        self.find_workspace(|ws| ws.id() == id)
    }

    pub fn find_workspace_by_name(&self, workspace_name: &str) -> Option<(usize, Workspace<W>)> {
        self.find_workspace(|ws| {
            ws.name(&self.containers)
                .is_some_and(|name| name.eq_ignore_ascii_case(workspace_name))
        })
    }

    fn ensure_workspace_by_name_impl(
        &mut self,
        workspace_name: &str,
        transient: bool,
    ) -> Option<(Option<Output>, usize)> {
        if let Some((idx, ws)) = self.find_workspace_by_name(workspace_name) {
            return Some((ws.current_output(&self.containers).cloned(), idx));
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
                let idx = mon.named_workspace_insert_idx(&self.containers);
                mon.add_workspace_at(&mut self.containers, idx);
                let lifetime = if transient {
                    WorkspaceLifetime::Transient
                } else {
                    WorkspaceLifetime::Persistent
                };
                let key = mon.ws(&self.containers, idx).workspace_key();
                self.containers
                    .workspace_properties_mut(key)
                    .set_name(name, lifetime);

                Some((Some(mon.output().clone()), idx))
            }
            MonitorSet::NoOutputs => {
                let idx = self.empty_outputless_workspace_idx();
                let lifetime = if transient {
                    WorkspaceLifetime::Transient
                } else {
                    WorkspaceLifetime::Persistent
                };
                let key = self.outputless_workspaces()[idx].workspace_key();
                self.containers
                    .workspace_properties_mut(key)
                    .set_name(name, lifetime);
                Some((None, idx))
            }
        }
    }

    pub fn find_workspace_by_number(&self, number: u32) -> Option<(usize, Workspace<W>)> {
        self.find_workspace(|ws| ws.numeric_number(&self.containers) == Some(number))
    }

    pub fn ensure_numeric_workspace(&mut self, number: u32) -> Option<(Option<Output>, usize)> {
        if let Some((idx, ws)) = self.find_workspace_by_number(number) {
            return Some((ws.current_output(&self.containers).cloned(), idx));
        }

        match &mut self.monitor_set {
            MonitorSet::Normal {
                monitors,
                active_monitor_idx,
                ..
            } => {
                let mon = &mut monitors[*active_monitor_idx];
                let idx = mon.add_numeric_workspace(
                    &mut self.containers,
                    number,
                    WorkspaceLifetime::Transient,
                );
                Some((Some(mon.output().clone()), idx))
            }
            MonitorSet::NoOutputs => {
                let idx = self.empty_outputless_workspace_idx();
                self.outputless_workspaces()[idx].set_numeric_identity(
                    &mut self.containers,
                    number,
                    WorkspaceLifetime::Transient,
                );
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

    pub fn find_workspace_by_ref(&self, reference: WorkspaceReference) -> Option<Workspace<W>> {
        if let WorkspaceReference::Index(index) = reference {
            return self.find_workspace_by_number(index).map(|(_, ws)| ws);
        }
        self.workspaces()
            .map(|(_, _, ws)| ws)
            .find(|ws| match &reference {
                WorkspaceReference::Name(ref_name) => ws
                    .name(&self.containers)
                    .is_some_and(|name| name.eq_ignore_ascii_case(ref_name)),
                WorkspaceReference::Id(id) => ws.id().get() == *id,
                WorkspaceReference::Index(_) => unreachable!(),
            })
    }

    /// The node of the workspace `reference` names, or of the active one.
    fn workspace_key_by_ref(&self, reference: Option<WorkspaceReference>) -> Option<NodeKey> {
        match reference {
            Some(reference) => self.find_workspace_by_ref(reference),
            None => self.active_workspace(),
        }
        .map(|ws| ws.workspace_key())
    }

    fn unname_workspace_by_id(&mut self, id: WorkspaceId) {
        let changed = match &mut self.monitor_set {
            MonitorSet::Normal { monitors, .. } => monitors
                .iter_mut()
                .any(|mon| mon.unname_workspace(&mut self.containers, id)),
            MonitorSet::NoOutputs => {
                let workspaces = self.outputless_workspaces();
                let Some(idx) = workspaces.iter().position(|ws| ws.id() == id) else {
                    return;
                };
                workspaces[idx].unname(&mut self.containers);

                // Clean up empty workspaces.
                if !self
                    .containers
                    .workspace_has_windows(workspaces[idx].workspace_key())
                {
                    workspaces[idx].remove_from_tree(&mut self.containers);
                }
                true
            }
        };
        if changed {
            self.seat_focus_after_mutation();
        }
    }

    pub fn find_window_and_output(&self, wl_surface: &WlSurface) -> Option<(&W, Option<&Output>)> {
        let key = self.containers.window_key_for_wl_surface(wl_surface)?;
        let window = self.containers.get_tile(key)?.window();
        Some((window, self.node_output(key)))
    }

    pub fn find_window_and_output_mut(
        &mut self,
        wl_surface: &WlSurface,
    ) -> Option<(&mut W, Option<&Output>)> {
        let key = self.containers.window_key_for_wl_surface(wl_surface)?;
        let output = Self::node_output_in(
            &self.monitor_set,
            &self.interactive_move,
            &self.containers,
            key,
        );
        let window = self.containers.get_tile_mut(key)?.window_mut();
        Some((window, output))
    }

    /// The output a node is shown on: its workspace's, or the drag's for a window being moved.
    /// None for the scratchpad and for workspaces without an output.
    fn node_output(&self, key: NodeKey) -> Option<&Output> {
        Self::node_output_in(
            &self.monitor_set,
            &self.interactive_move,
            &self.containers,
            key,
        )
    }

    /// [`Self::node_output`] over the fields it reads, for callers holding the tree mutably.
    fn node_output_in<'a>(
        monitor_set: &'a MonitorSet<W>,
        interactive_move: &'a Option<InteractiveMoveState<W>>,
        containers: &ContainerTree<W>,
        key: NodeKey,
    ) -> Option<&'a Output> {
        if let Some(move_) = interactive_move
            .as_ref()
            .and_then(InteractiveMoveState::moving)
        {
            if move_.node == key {
                return Some(&move_.output);
            }
        }
        let output = containers.workspace_output_key(containers.workspace_of(key)?)?;
        let MonitorSet::Normal { monitors, .. } = monitor_set else {
            return None;
        };
        monitors
            .iter()
            .find(|mon| mon.output_key == output)
            .map(|mon| &mon.output)
    }

    /// Computes the window-geometry-relative target rect for popup unconstraining.
    ///
    /// We will try to fit popups inside this rect.
    ///
    /// If `overflow` is true, return the whole working area even for a tiled window, whose popups
    /// are otherwise kept within the window's width. This is used for IME popups, which people
    /// expect to extend past the window, and which go away when focus changes anyway.
    pub fn popup_target_rect(&self, window: &W::Id, overflow: bool) -> Rectangle<f64, Logical> {
        if let Some(InteractiveMoveState::Moving(move_)) = &self.interactive_move {
            if &move_.window_id == window {
                let tile = self
                    .containers
                    .transferred_tile(move_.node)
                    .expect("moving view must remain transferred");
                let output_size = output_size(&move_.output);
                // FIXME: ideally this shouldn't include the tile render offset, but the code
                // duplication would be a bit annoying for this edge case.
                let tile_pos = move_.tile_render_location(tile, 1.);
                let target = if overflow {
                    let mut target = Rectangle::from_size(output_size);
                    target.loc -= tile_pos;
                    target.loc -= tile.window_loc();
                    target
                } else {
                    // Follow the tiling layout logic and fit the popup horizontally within the
                    // window geometry.
                    let width = tile.window_size().w;
                    let mut target = Rectangle::from_size(Size::from((width, output_size.h)));
                    target.loc.y -= tile_pos.y;
                    target.loc.y -= tile.window_loc().y;
                    target
                };
                return target;
            }
        }

        self.workspaces()
            .find_map(|(_, _, ws)| ws.popup_target_rect(&self.containers, window, overflow))
            .unwrap()
    }

    pub fn update_output_size(&mut self, output: &Output) {
        let _span = tracy_client::span!("Layout::update_output_size");

        let Some((mon, containers)) = self.monitor_for_output_and_containers_mut(output) else {
            error!("monitor missing in update_output_size()");
            return;
        };

        mon.update_output_size(containers);
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

        let Some((monitor_idx, workspace_idx)) = self.monitor_workspace_indices_for_window(window)
        else {
            return true;
        };
        let mon = &monitors[monitor_idx];

        // During a gesture, focus-follows-mouse does not cause any unintended workspace switches.
        if let Some(WorkspaceSwitch::Gesture(_)) = mon.workspace_switch {
            return true;
        }

        workspace_idx == mon.active_workspace_idx(&self.containers)
    }

    pub fn activate_window(&mut self, window: &W::Id) {
        self.activate_window_with_raise(window, true);
    }

    pub fn activate_window_without_raising(&mut self, window: &W::Id) {
        self.activate_window_with_raise(window, false);
    }

    fn activate_window_with_raise(&mut self, window: &W::Id, raise: bool) {
        if self
            .interactive_move
            .as_ref()
            .and_then(InteractiveMoveState::moving_window_id)
            .is_some_and(|id| id == window)
        {
            return;
        }

        let Some((monitor_idx, workspace_idx)) = self.monitor_workspace_indices_for_window(window)
        else {
            return;
        };
        let MonitorSet::Normal {
            monitors,
            active_monitor_idx,
            ..
        } = &mut self.monitor_set
        else {
            return;
        };

        let mon = &mut monitors[monitor_idx];
        let ws = mon.ws(&self.containers, workspace_idx);
        let activated = if raise {
            ws.activate_window(&mut self.containers, window)
        } else {
            ws.activate_window_without_raising(&mut self.containers, window)
        };
        if !activated {
            return;
        }
        *active_monitor_idx = monitor_idx;

        // If currently in the middle of a vertical swipe between the target workspace and some
        // other, don't switch the workspace.
        match &mon.workspace_switch {
            Some(WorkspaceSwitch::Gesture(gesture))
                if gesture.current_idx.floor() == workspace_idx as f64
                    || gesture.current_idx.ceil() == workspace_idx as f64 => {}
            _ => mon.switch_workspace(&mut self.containers, WorkspaceTarget::Index(workspace_idx)),
        }

        self.seat_focus_record_active_chain();
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

    pub fn active_workspace(&self) -> Option<Workspace<W>> {
        let MonitorSet::Normal {
            monitors,
            active_monitor_idx,
            ..
        } = &self.monitor_set
        else {
            return None;
        };

        Some(monitors[*active_monitor_idx].active_workspace(&self.containers))
    }

    /// The active workspace and the tree it lives in.
    #[cfg(test)]
    pub(crate) fn active_workspace_and_containers(
        &self,
    ) -> Option<(Workspace<W>, &ContainerTree<W>)> {
        Some((self.active_workspace()?, &self.containers))
    }

    pub(crate) fn active_workspace_and_containers_mut(
        &mut self,
    ) -> Option<(Workspace<W>, &mut ContainerTree<W>)> {
        Some((self.active_workspace()?, &mut self.containers))
    }

    /// Which monitor shows the workspace `root`, and where among its workspaces.
    ///
    /// `None` for the scratchpad and for workspaces waiting for an output. The index is read
    /// fresh from the output's list each time and is never kept.
    fn monitor_location(&self, root: NodeKey) -> Option<(usize, usize)> {
        let output = self.containers.workspace_output_key(root)?;
        let MonitorSet::Normal { monitors, .. } = &self.monitor_set else {
            return None;
        };
        let monitor_idx = monitors.iter().position(|mon| mon.output_key == output)?;
        let workspace_idx = self
            .containers
            .output_workspaces(output)
            .iter()
            .position(|&key| key == root)?;
        Some((monitor_idx, workspace_idx))
    }

    /// Where on `output` the workspace holding `window` sits, if it is still on that output.
    ///
    /// The overview's tap target. The output is re-checked because the workspace may have moved
    /// to another one since the gesture began.
    pub fn workspace_idx_on_output_for_window(
        &self,
        output: &Output,
        window: &W::Id,
    ) -> Option<usize> {
        let (monitor_idx, workspace_idx) = self.monitor_workspace_indices_for_window(window)?;
        let MonitorSet::Normal { monitors, .. } = &self.monitor_set else {
            return None;
        };
        (monitors[monitor_idx].output == *output).then_some(workspace_idx)
    }

    fn monitor_workspace_indices_for_window(&self, window: &W::Id) -> Option<(usize, usize)> {
        let tree = &self.containers;
        self.monitor_location(
            tree.window_key(window)
                .and_then(|key| tree.workspace_of(key))?,
        )
    }

    /// Transfer one leaf for reparenting without taking it out of the layout-wide tree.
    fn take_window_for_transfer(
        &mut self,
        window: &W::Id,
        transaction: Transaction,
    ) -> Option<ViewTransfer> {
        let tree = &self.containers;
        let workspace = Workspace::of(
            tree.window_key(window)
                .and_then(|key| tree.workspace_of(key))?,
        );
        let transferred =
            workspace.take_tile_for_transfer(&mut self.containers, window, transaction);
        self.workspace_lost_window(workspace);
        Some(transferred)
    }

    fn window_is_floating(&self, window: &W::Id) -> bool {
        let tree = &self.containers;
        tree.window_key(window)
            .is_some_and(|key| tree.is_in_floating_branch(key))
    }

    /// The workspaces waiting for an output, in order.
    fn outputless_workspaces(&self) -> Vec<Workspace<W>> {
        self.containers
            .output_workspaces(self.fallback_output)
            .iter()
            .map(|&key| Workspace::of(key))
            .collect()
    }

    /// Where `workspace` is drawn on its output, or `None` while it waits for one.
    fn workspace_render_loc(&self, workspace: Workspace<W>) -> Option<Point<f64, Logical>> {
        let output = self
            .containers
            .workspace_output_key(workspace.workspace_key())?;
        let MonitorSet::Normal { monitors, .. } = &self.monitor_set else {
            return None;
        };
        let mon = monitors.iter().find(|mon| mon.output_key == output)?;
        let idx = mon.idx_of_ws(&self.containers, workspace.id())?;
        mon.workspaces_render_geo(&self.containers)
            .nth(idx)
            .map(|geo| geo.loc)
    }

    /// A window has just left `workspace`.
    ///
    /// On an output, the output keeps its empty workspaces at the ends (unless a switch is
    /// showing them). Waiting for one, a workspace holding nothing worth keeping goes, since
    /// none of the workspaces waiting for an output may be empty.
    fn workspace_lost_window(&mut self, workspace: Workspace<W>) {
        let Some(output) = self
            .containers
            .workspace_output_key(workspace.workspace_key())
        else {
            return;
        };
        if output == self.fallback_output {
            if !workspace.has_windows_or_persistent_identity(&self.containers) {
                workspace.remove_from_tree(&mut self.containers);
            }
            return;
        }
        let MonitorSet::Normal { monitors, .. } = &mut self.monitor_set else {
            return;
        };
        if let Some(mon) = monitors.iter_mut().find(|mon| mon.output_key == output) {
            if mon.workspace_switch.is_none() {
                mon.clean_up_workspaces(&mut self.containers);
            }
        }
    }

    /// Put `ws` last among the workspaces waiting for an output; returns where it is.
    fn push_outputless_workspace(&mut self, ws: Workspace<W>) -> usize {
        let idx = self
            .containers
            .output_workspaces(self.fallback_output)
            .len();
        ws.attach(&mut self.containers, self.fallback_output, idx);
        idx
    }

    /// The first workspace waiting for an output, made if there is none.
    fn first_outputless_workspace(&mut self) -> Workspace<W> {
        if let Some(&key) = self
            .containers
            .output_workspaces(self.fallback_output)
            .first()
        {
            return Workspace::of(key);
        }
        let ws = Workspace::new_no_outputs(&mut self.containers, self.options.clone());
        self.push_outputless_workspace(ws);
        ws
    }

    /// The index of a workspace waiting for an output that holds nothing worth keeping, made
    /// if there is none.
    fn empty_outputless_workspace_idx(&mut self) -> usize {
        if let Some(idx) = self
            .outputless_workspaces()
            .iter()
            .position(|ws| !ws.has_windows_or_persistent_identity(&self.containers))
        {
            return idx;
        }
        let ws = Workspace::new_no_outputs(&mut self.containers, self.options.clone());
        self.push_outputless_workspace(ws)
    }

    /// A normal output-backed or outputless workspace holding `window`.
    ///
    /// The scratchpad deliberately does not answer: callers of this helper operate on a
    /// workspace of the monitor set, which a hidden window is not on.
    fn workspace_for_window(&self, window: &W::Id) -> Option<Workspace<W>> {
        let tree = &self.containers;
        let root = tree
            .window_key(window)
            .and_then(|key| tree.workspace_of(key))?;
        (root != self.scratchpad.workspace_key()).then(|| Workspace::of(root))
    }

    fn workspace_for_window_and_containers_mut(
        &mut self,
        window: &W::Id,
    ) -> Option<(Workspace<W>, &mut ContainerTree<W>)> {
        Some((self.workspace_for_window(window)?, &mut self.containers))
    }

    pub(crate) fn workspace_by_id_and_containers_mut(
        &mut self,
        workspace_id: WorkspaceId,
    ) -> Option<(Workspace<W>, &mut ContainerTree<W>)> {
        let (_, workspace) = self.find_workspace_by_id(workspace_id)?;
        Some((workspace, &mut self.containers))
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
            .and_then(|move_| self.containers.transferred_tile(move_.node))
            .map(Tile::window)
            .into_iter();

        let mon = monitors.iter().find(|mon| &mon.output == output).unwrap();
        let mon_windows = mon
            .workspaces(&self.containers)
            .flat_map(|ws| self.containers.windows_in_workspace(ws.workspace_key()));

        moving_window.chain(mon_windows)
    }

    pub fn windows_for_output_mut(&mut self, output: &Output) -> impl Iterator<Item = &mut W> + '_ {
        let MonitorSet::Normal { monitors, .. } = &self.monitor_set else {
            panic!("windows_for_output_mut requires connected outputs")
        };
        let moving = self
            .interactive_move
            .as_ref()
            .and_then(InteractiveMoveState::moving)
            .filter(|move_| move_.output == *output);
        let mon = monitors.iter().find(|mon| &mon.output == output).unwrap();
        let roots: Vec<_> = mon
            .workspaces(&self.containers)
            .map(|ws| ws.workspace_key())
            .collect();
        let mut keys: Vec<_> = roots
            .into_iter()
            .flat_map(|root| self.containers.leaf_keys_in_workspace(root))
            .collect();
        if let Some(move_) = moving {
            keys.push(move_.node);
        }
        self.containers
            .tiles_mut_for_keys(&keys)
            .into_iter()
            .map(|(_, tile)| tile.window_mut())
    }

    pub fn with_windows(
        &self,
        mut f: impl FnMut(&W, Option<&Output>, Option<WorkspaceId>, WindowLayout),
    ) {
        if let Some(InteractiveMoveState::Moving(move_)) = &self.interactive_move {
            // We don't fill any positions for interactively moved windows.
            let tile = self
                .containers
                .transferred_tile(move_.node)
                .expect("moving view must remain transferred");
            let layout = tile.ipc_layout_template();
            f(tile.window(), Some(&move_.output), None, layout);
        }

        for tile in self
            .containers
            .tiles_in_workspace(self.scratchpad.workspace_key())
        {
            let layout = tile.ipc_layout_template();
            f(tile.window(), None, None, layout);
        }

        for (_, _, ws) in self.workspaces() {
            let output = ws.current_output(&self.containers);
            for (tile, layout) in self
                .containers
                .workspace_tiles_with_ipc_layouts(ws.workspace_key())
            {
                f(tile.window(), output, Some(ws.id()), layout);
            }
        }
    }

    pub fn with_windows_mut(&mut self, mut f: impl FnMut(&mut W, Option<&Output>)) {
        if let Some(InteractiveMoveState::Moving(move_)) = &self.interactive_move {
            let window = self
                .containers
                .transferred_tile_mut(move_.node)
                .expect("moving view must remain transferred")
                .window_mut();
            f(window, Some(&move_.output));
        }

        let scratchpad = self.scratchpad.workspace_key();
        for tile in self.containers.tiles_mut_in_workspace(scratchpad) {
            f(tile.window_mut(), None);
        }

        for ws in self.workspace_handles() {
            let output = ws.current_output(&self.containers).cloned();
            for win in self.containers.windows_mut_in_workspace(ws.workspace_key()) {
                f(win, output.as_ref());
            }
        }
    }

    fn active_monitor_and_containers_mut(
        &mut self,
    ) -> Option<(&mut Monitor<W>, &mut ContainerTree<W>)> {
        let containers = &mut self.containers;
        let MonitorSet::Normal {
            monitors,
            active_monitor_idx,
            ..
        } = &mut self.monitor_set
        else {
            return None;
        };

        Some((&mut monitors[*active_monitor_idx], containers))
    }

    fn monitor_for_window_and_containers_mut(
        &mut self,
        window: &W::Id,
    ) -> Option<(&mut Monitor<W>, &mut ContainerTree<W>)> {
        let (monitor_idx, _) = self.monitor_workspace_indices_for_window(window)?;
        let containers = &mut self.containers;
        let MonitorSet::Normal { monitors, .. } = &mut self.monitor_set else {
            return None;
        };
        let monitor = monitors.get_mut(monitor_idx)?;
        Some((monitor, containers))
    }

    fn monitor_for_output_and_containers_mut(
        &mut self,
        output: &Output,
    ) -> Option<(&mut Monitor<W>, &mut ContainerTree<W>)> {
        let containers = &mut self.containers;
        let MonitorSet::Normal { monitors, .. } = &mut self.monitor_set else {
            return None;
        };
        let monitor = monitors.iter_mut().find(|mon| &mon.output == output)?;
        Some((monitor, containers))
    }

    /// The tree every workspace lives in, for the calls on a monitor or workspace that read it.
    pub fn containers(&self) -> &ContainerTree<W> {
        &self.containers
    }

    /// Run a focus command on the active workspace and re-record the seat focus chain
    /// afterwards, so individual commands cannot forget to.
    fn with_active_workspace_focus<R>(
        &mut self,
        f: impl FnOnce(Workspace<W>, &mut ContainerTree<W>) -> R,
    ) {
        let Some((workspace, containers)) = self.active_workspace_and_containers_mut() else {
            return;
        };
        let _ = f(workspace, containers);
        self.seat_focus_record_active_chain();
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

    /// The monitors, paired with the tree their workspaces live in.
    ///
    /// One accessor for both: an iterator that borrowed all of `self` leaves nothing to hand
    /// a monitor when it needs the tree.
    fn monitors_and_containers_mut(
        &mut self,
    ) -> (
        impl Iterator<Item = &mut Monitor<W>> + '_,
        &mut ContainerTree<W>,
    ) {
        let containers = &mut self.containers;
        let monitors = match &mut self.monitor_set {
            MonitorSet::Normal { monitors, .. } => Some(monitors.iter_mut()),
            MonitorSet::NoOutputs => None,
        };
        (monitors.into_iter().flatten(), containers)
    }

    fn end_dnd_scroll_gestures(&mut self) {
        let (monitors, containers) = self.monitors_and_containers_mut();
        for mon in monitors {
            mon.dnd_scroll_gesture_end(containers);
        }
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
        let Some((mon, containers)) = self.monitor_for_output_and_containers_mut(output) else {
            return false;
        };
        if !mon.update_layout_config(containers, layout_config) {
            return false;
        }
        self.seat_focus_after_mutation();
        true
    }

    pub fn monitor_for_output(&self, output: &Output) -> Option<&Monitor<W>> {
        self.monitors().find(|mon| &mon.output == output)
    }

    fn find_workspace_location_by_id(&self, id: WorkspaceId) -> Option<(usize, usize)> {
        match &self.monitor_set {
            MonitorSet::Normal { monitors, .. } => {
                monitors.iter().enumerate().find_map(|(monitor_idx, mon)| {
                    mon.workspaces(&self.containers)
                        .enumerate()
                        .find(|(_, ws)| ws.id() == id)
                        .map(|(workspace_idx, _)| (monitor_idx, workspace_idx))
                })
            }
            MonitorSet::NoOutputs => None,
        }
    }

    /// Record the active workspace as the one the seat is on.
    fn seat_focus_record_active_chain(&mut self) {
        if !self.has_layout_focus {
            return;
        }
        let active = match &self.monitor_set {
            MonitorSet::Normal {
                monitors,
                active_monitor_idx,
                ..
            } => monitors[*active_monitor_idx]
                .active_workspace_ref(&self.containers)
                .workspace_key(),
            MonitorSet::NoOutputs => {
                let workspaces = self.outputless_workspaces();
                let Some(ws) = workspaces.first() else {
                    return;
                };
                ws.workspace_key()
            }
        };
        self.containers.raise_workspace(active);
    }

    fn seat_focus_record_workspace_chain(&mut self, workspace_id: WorkspaceId) {
        if !self.has_layout_focus {
            return;
        }
        if let Some((_, workspace)) = self.find_workspace_by_id(workspace_id) {
            let key = workspace.workspace_key();
            self.containers.raise_workspace(key);
        }
    }

    fn seat_focus_workspace_targets_window(
        &self,
        workspace_id: WorkspaceId,
        window_id: &W::Id,
    ) -> bool {
        self.find_workspace_by_id(workspace_id)
            .is_some_and(|(_, workspace)| {
                workspace.focus_targets_window(&self.containers, window_id)
            })
    }

    fn seat_focus_after_mutation(&mut self) {
        self.seat_focus_record_active_chain();
    }

    /// Show on `output` the workspace of it the seat was most recently on.
    fn seat_focus_restore_output(&mut self, output: &Output) {
        let MonitorSet::Normal {
            monitors,
            active_monitor_idx,
            ..
        } = &mut self.monitor_set
        else {
            return;
        };
        let Some(monitor_idx) = monitors.iter().position(|mon| mon.output == *output) else {
            return;
        };
        let monitor = &monitors[monitor_idx];
        let Some(workspace_idx) = self
            .containers
            .most_recent_workspace(|key| {
                monitor
                    .workspaces(&self.containers)
                    .any(|ws| ws.workspace_key() == key)
            })
            .and_then(|key| {
                monitor
                    .workspaces(&self.containers)
                    .position(|ws| ws.workspace_key() == key)
            })
        else {
            return;
        };

        *active_monitor_idx = monitor_idx;
        monitors[monitor_idx]
            .switch_workspace(&mut self.containers, WorkspaceTarget::Index(workspace_idx));
        let key = monitors[monitor_idx]
            .active_workspace(&self.containers)
            .workspace_key();
        self.containers.raise_workspace(key);
    }

    pub fn set_seat_layout_focus(&mut self, has_layout_focus: bool) {
        self.has_layout_focus = has_layout_focus;
        if has_layout_focus {
            self.seat_focus_record_active_chain();
        }
    }

    pub fn refresh_seat_focus(&mut self) {
        self.seat_focus_after_mutation();
    }

    pub fn monitor_for_workspace(&self, workspace_name: &str) -> Option<&Monitor<W>> {
        self.monitors().find(|monitor| {
            monitor.workspaces(&self.containers).any(|ws| {
                ws.name(&self.containers)
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
    /// sway's `move <direction>` on the active workspace.
    pub fn move_in_direction(&mut self, direction: Direction) {
        self.request_refresh();
        if let Some((workspace, containers)) = self.active_workspace_and_containers_mut() {
            workspace.move_in_direction(containers, direction);
        }
    }

    /// Move in `direction`, or, when there is nowhere to go in the workspace, to the next
    /// output that way. Returns whether the move went to the output.
    pub fn move_in_direction_or_to_output(
        &mut self,
        direction: Direction,
        output: &Output,
    ) -> bool {
        if let Some((workspace, containers)) = self.active_workspace_and_containers_mut() {
            if workspace.move_in_direction(containers, direction) {
                return false;
            }
        }

        self.transfer(
            TransferSubject::Container,
            TransferTarget::Output {
                output,
                workspace: None,
            },
            ActivateWindow::Yes,
        );
        true
    }
    /// Move the root container holding the focus to another position among its siblings.
    pub fn move_root_container(&mut self, position: RootPosition) {
        let Some((workspace, containers)) = self.active_workspace_and_containers_mut() else {
            return;
        };
        workspace.move_root_container(containers, position);
    }
    /// Move up or down in the active workspace, or at its edge to the workspace that way.
    pub fn move_vertically_or_to_workspace(&mut self, direction: Direction) {
        let Some((monitor, containers)) = self.active_monitor_and_containers_mut() else {
            return;
        };
        monitor.move_vertically_or_to_workspace(containers, direction);
    }
    pub fn consume_or_expel_window(&mut self, window: Option<&W::Id>, direction: Direction) {
        self.request_refresh();
        if let Some(InteractiveMoveState::Moving(move_)) = &self.interactive_move {
            if window.is_none() || window == Some(&move_.window_id) {
                return;
            }
        }

        let workspace = if let Some(window) = window {
            self.workspace_for_window_and_containers_mut(window)
        } else {
            self.active_workspace_and_containers_mut()
        };
        let Some((workspace, containers)) = workspace else {
            return;
        };
        workspace.consume_or_expel_window(containers, window, direction);
    }
    /// Run a focus command on the active workspace.
    pub fn focus_command(&mut self, command: FocusCommand) {
        self.with_active_workspace_focus(|ws, containers| ws.focus_command(containers, command));
    }

    /// Focus in `direction` without wrapping, or, when nothing is there, the next output that
    /// way. Returns whether focus went to the output.
    pub fn focus_in_direction_or_output(&mut self, direction: Direction, output: &Output) -> bool {
        if let Some((workspace, containers)) = self.active_workspace_and_containers_mut() {
            if workspace.focus_in_direction(containers, direction, false) {
                self.seat_focus_record_active_chain();
                return false;
            }
        }

        self.focus_output_in_direction_internal(output, direction);
        self.seat_focus_record_active_chain();
        true
    }

    /// Focus up or down in the active workspace, or at its edge switch to the workspace that
    /// way.
    pub fn focus_vertically_or_workspace(&mut self, direction: Direction) {
        let Some((monitor, containers)) = self.active_monitor_and_containers_mut() else {
            return;
        };
        monitor.focus_vertically_or_workspace(containers, direction);
        self.seat_focus_record_active_chain();
    }
    /// Move a window, the selected container or the root container holding the focus to
    /// another workspace or output.
    ///
    /// `None` when the target workspace does not exist; otherwise the output the subject went
    /// to, when it left the one it was on for another.
    pub fn transfer(
        &mut self,
        subject: TransferSubject<'_, W::Id>,
        target: TransferTarget<'_>,
        activate: ActivateWindow,
    ) -> Option<Option<Output>> {
        self.request_refresh();
        if let TransferSubject::Window(window) = subject {
            if let Some(InteractiveMoveState::Moving(move_)) = &self.interactive_move {
                if window.is_none() || window == Some(&move_.window_id) {
                    return Some(None);
                }
            }
        }

        match target {
            TransferTarget::Workspace(workspace_id) => {
                let (idx, ws) = self.find_workspace_by_id(workspace_id)?;
                let output = ws.current_output(&self.containers).cloned();
                // A window named by id keeps its target output even when that output is the
                // active one, so the move is right whichever output the window is on. Anything
                // aimed at the focus is on the active output, and stays there when it can.
                let stays_on_output = match subject {
                    TransferSubject::Window(Some(_)) => false,
                    _ => output.as_ref() == self.active_output(),
                };
                match output.filter(|_| !stays_on_output) {
                    Some(output) => {
                        let target = TransferTarget::Output {
                            output: &output,
                            workspace: Some(idx),
                        };
                        self.transfer(subject, target, activate);
                        Some(Some(output))
                    }
                    None => self.transfer(subject, TransferTarget::WorkspaceIndex(idx), activate),
                }
            }
            TransferTarget::Output { output, workspace } => {
                let activate_container = !matches!(activate, ActivateWindow::No);
                match subject {
                    TransferSubject::Window(window) => {
                        self.move_to_output(window, output, workspace, activate)
                    }
                    TransferSubject::Container => self.move_tiling_target_to_output(
                        output,
                        workspace,
                        activate_container,
                        false,
                    ),
                    TransferSubject::RootContainer => self.move_tiling_target_to_output(
                        output,
                        workspace,
                        activate_container,
                        true,
                    ),
                }
                Some(None)
            }
            TransferTarget::WorkspaceUp
            | TransferTarget::WorkspaceDown
            | TransferTarget::WorkspaceIndex(_) => {
                let monitor = match subject {
                    TransferSubject::Window(Some(window)) => {
                        self.monitor_for_window_and_containers_mut(window)
                    }
                    _ => self.active_monitor_and_containers_mut(),
                };
                let (monitor, containers) = monitor?;
                let idx = monitor.workspace_idx_for(&*containers, target)?;
                monitor.transfer_to_workspace(containers, subject, idx, activate);
                self.seat_focus_after_mutation();
                Some(None)
            }
        }
    }
    /// Show another workspace on the active output.
    pub fn switch_workspace(&mut self, target: WorkspaceTarget) {
        let Some((monitor, containers)) = self.active_monitor_and_containers_mut() else {
            return;
        };
        monitor.switch_workspace(containers, target);
        self.seat_focus_record_active_chain();
    }

    /// Show another workspace on `output`, without focusing that output.
    pub fn switch_workspace_on_output(&mut self, output: &Output, target: WorkspaceTarget) -> bool {
        let Some((monitor, containers)) = self.monitor_for_output_and_containers_mut(output) else {
            return false;
        };
        monitor.switch_workspace(containers, target);
        true
    }

    pub fn focus_workspace_by_id(
        &mut self,
        workspace_id: WorkspaceId,
        auto_back_and_forth: bool,
    ) -> Option<Option<Output>> {
        let (idx, mut output) = {
            let (idx, ws) = self.find_workspace_by_id(workspace_id)?;
            (idx, ws.current_output(&self.containers).cloned())
        };

        if let Some(active) = self.active_output() {
            if output.as_ref() == Some(active) {
                output = None;
            }
        }

        if let Some(target_output) = output.as_ref() {
            self.focus_output(target_output);
        }
        let (monitor, containers) = self.active_monitor_and_containers_mut()?;
        let target = if output.is_none() && auto_back_and_forth {
            WorkspaceTarget::IndexOrBack(idx)
        } else {
            WorkspaceTarget::Index(idx)
        };
        monitor.switch_workspace(containers, target);

        self.seat_focus_record_active_chain();

        Some(output)
    }

    /// sway's `swap container with`, addressed by window rather than by direction.
    ///
    /// Only within the active workspace, which is where sway's own refusals leave the
    /// interesting cases anyway; a target on another workspace is left alone rather than
    /// dragged across one.
    pub fn swap_window_with(&mut self, target: &W::Id) -> bool {
        let Some(source_workspace) = self.active_workspace().map(|ws| ws.workspace_key()) else {
            return false;
        };
        self.containers
            .swap_selected_with_window(source_workspace, target)
    }

    pub fn center_window(&mut self, id: Option<&W::Id>) {
        if let Some(InteractiveMoveState::Moving(move_)) = &mut self.interactive_move {
            if id.is_none() || id == Some(&move_.window_id) {
                return;
            }
        }

        let workspace = if let Some(id) = id {
            self.workspace_for_window_and_containers_mut(id)
        } else {
            self.active_workspace_and_containers_mut()
        };

        let Some((workspace, containers)) = workspace else {
            return;
        };
        workspace.center_window(containers, id);
    }

    pub fn focus(&self) -> Option<&W> {
        self.focus_with_output().map(|(win, _out)| win)
    }

    pub fn close_window_ids_for_active_selection(&self) -> Vec<W::Id> {
        if let Some(workspace) = self.active_workspace() {
            let ids = workspace.close_window_ids_for_active_selection(&self.containers);
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
            .is_some_and(|ws| ws.active_selection_is_container(&self.containers))
    }

    pub fn active_command_can_fullscreen(&self) -> bool {
        self.active_workspace().is_some_and(|ws| {
            self.containers
                .workspace_command_node(ws.workspace_key())
                .is_some()
        })
    }

    pub fn focus_with_output(&self) -> Option<(&W, &Output)> {
        if let Some(InteractiveMoveState::Moving(move_)) = &self.interactive_move {
            let window = self
                .containers
                .transferred_tile(move_.node)
                .expect("moving view must remain transferred")
                .window();
            return Some((window, &move_.output));
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
        mon.active_window(&self.containers)
            .map(|win| (win, &mon.output))
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
                let tile = self
                    .containers
                    .transferred_tile(move_.node)
                    .expect("moving view must remain transferred");
                if self.overview_progress.is_some() {
                    let zoom = self.overview_zoom();
                    let tile_pos = move_.tile_render_location(tile, zoom);
                    let pos_within_tile = (pos_within_output - tile_pos).downscale(zoom);
                    // During the overview animation, we cannot do input hits because we cannot
                    // really represent scaled windows properly.
                    let (win, hit) =
                        HitType::hit_tile(tile, Point::from((0., 0.)), pos_within_tile)?;
                    return Some((win, hit.to_activate()));
                } else {
                    let tile_pos = move_.tile_render_location(tile, 1.);
                    return HitType::hit_tile(tile, tile_pos, pos_within_output);
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

            let (monitor_idx, workspace_idx) =
                self.monitor_workspace_indices_for_window(&move_.window_id)?;
            let mon = self.monitors().nth(monitor_idx)?;
            let (ws, ws_geo) = mon
                .workspaces_with_render_geo(&self.containers)
                .nth(workspace_idx)?;
            if mon.output() != output {
                return None;
            }

            let (tile, tile_offset, _visible) = ws
                .tiles_with_render_positions(&self.containers)
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
        mon.window_under(&self.containers, pos_within_output)
    }

    #[cfg(test)]
    pub fn resize_edges_under(
        &self,
        output: &Output,
        pos_within_output: Point<f64, Logical>,
    ) -> Option<ResizeEdge> {
        self.resize_hit_under(output, pos_within_output)
            .map(|hit| hit.edges)
    }

    pub fn resize_hit_under(
        &self,
        output: &Output,
        pos_within_output: Point<f64, Logical>,
    ) -> Option<ResizeHit<W::Id>> {
        let mon = self.monitor_for_output(output)?;
        mon.resize_hit_under(&self.containers, pos_within_output)
    }

    pub fn workspace_under(
        &self,
        extended_bounds: bool,
        output: &Output,
        pos_within_output: Point<f64, Logical>,
    ) -> Option<Workspace<W>> {
        if self
            .interactive_moved_window_under(output, pos_within_output)
            .is_some()
        {
            return None;
        }

        let mon = self.monitor_for_output(output)?;
        if extended_bounds {
            mon.workspace_under(&self.containers, pos_within_output)
                .map(|(ws, _)| ws)
        } else {
            mon.workspace_under_narrow(&self.containers, pos_within_output)
        }
    }

    pub fn overview_zoom(&self) -> f64 {
        let progress = self.overview_progress.as_ref().map(|p| p.value());
        compute_overview_zoom(&self.options, progress)
    }

    /// True while any workspace is still waiting on a configure it sent.
    pub(crate) fn has_pending_layouts(&self) -> bool {
        self.workspaces()
            .any(|(_, _, ws)| self.containers.has_pending_layouts(ws.workspace_key()))
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

        let zoom = self.overview_zoom();

        let mut move_win_id = None;
        let mut interactive_transfer = None;
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
                    interactive_transfer = Some(move_.node);
                    let tile = self
                        .containers
                        .transferred_tile(move_.node)
                        .expect("moving view must remain transferred");
                    assert_eq!(self.clock, tile.clock);
                    assert!(tile.window().pending_sizing_mode().is_normal());

                    tile.verify_invariants();

                    let scale = move_.output.current_scale().fractional_scale();
                    let options = Options::clone(&self.options)
                        .with_merged_layout(move_.output_config.as_ref())
                        .with_merged_layout(move_.workspace_config.as_ref().map(|(_, c)| c))
                        .adjusted_for_scale(scale);
                    assert_eq!(
                        &*tile.options, &options,
                        "interactive moved tile options must be \
                         base options adjusted for output scale"
                    );

                    let tile_pos = move_.tile_render_location(tile, zoom);
                    let rounded_pos = tile_pos.to_physical_precise_round(scale).to_logical(scale);

                    // Tile position must be rounded to physical pixels.
                    assert_abs_diff_eq!(tile_pos.x, rounded_pos.x, epsilon = 1e-5);
                    assert_abs_diff_eq!(tile_pos.y, rounded_pos.y, epsilon = 1e-5);

                    if let Some(alpha) = &tile.alpha_animation {
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

        let transfer_roots: Vec<_> = self.containers.transfer_root_keys().collect();
        match interactive_transfer {
            Some(node) => assert_eq!(
                transfer_roots,
                vec![node],
                "only the active interactive move may survive as an in-flight tree transfer",
            ),
            None => assert!(
                transfer_roots.is_empty(),
                "a completed synchronous reparent must not leave an in-flight root: \
                 {transfer_roots:?}",
            ),
        }

        self.containers.verify_outputs();
        let mut runtime_workspace_roots = HashSet::new();
        assert!(runtime_workspace_roots.insert(self.scratchpad.workspace_key()));
        assert!(
            self.scratchpad.current_output(&self.containers).is_none(),
            "the scratchpad is on no output"
        );
        match &self.monitor_set {
            MonitorSet::Normal { monitors, .. } => {
                assert!(
                    self.outputless_workspaces().is_empty(),
                    "with an output connected no workspace waits on the fallback"
                );
                for monitor in monitors {
                    for workspace in monitor.workspaces(&self.containers) {
                        assert!(runtime_workspace_roots.insert(workspace.workspace_key()));
                        assert_eq!(
                            workspace.current_output(&self.containers),
                            Some(&monitor.output),
                            "a monitor's workspaces must be on its output"
                        );
                    }
                }
            }
            MonitorSet::NoOutputs => {
                let workspaces = self.outputless_workspaces();
                for workspace in workspaces {
                    assert!(runtime_workspace_roots.insert(workspace.workspace_key()));
                    assert!(
                        workspace.current_output(&self.containers).is_none(),
                        "without outputs no workspace is on one"
                    );
                }
            }
        }
        let tree_workspace_roots: HashSet<_> =
            self.containers.workspace_keys().into_iter().collect();
        assert_eq!(
            runtime_workspace_roots, tree_workspace_roots,
            "layout runtimes and tree workspace roots must form a bijection",
        );

        let mut seen_workspace_id = HashSet::new();
        let mut seen_workspace_name = Vec::<String>::new();

        let (monitors, &primary_idx, &active_monitor_idx) = match &self.monitor_set {
            MonitorSet::Normal {
                monitors,
                primary_idx,
                active_monitor_idx,
            } => (monitors, primary_idx, active_monitor_idx),
            MonitorSet::NoOutputs => {
                let workspaces = self.outputless_workspaces();
                for workspace in workspaces {
                    assert!(
                        workspace.has_windows_or_persistent_identity(&self.containers),
                        "with no outputs there cannot be empty unnamed workspaces"
                    );

                    assert_eq!(
                        workspace.properties(&self.containers).base_options,
                        self.options,
                        "workspace base options must be synchronized with layout"
                    );

                    assert!(
                        seen_workspace_id.insert(workspace.id()),
                        "workspace id must be unique"
                    );

                    if let Some(name) = workspace.name(&self.containers) {
                        assert!(
                            !seen_workspace_name
                                .iter()
                                .any(|n| n.eq_ignore_ascii_case(name)),
                            "workspace name must be unique"
                        );
                        seen_workspace_name.push(name.clone());
                    }

                    workspace.verify_invariants(&self.containers, move_win_id.as_ref());
                }

                return;
            }
        };

        assert!(primary_idx < monitors.len());
        assert!(active_monitor_idx < monitors.len());

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

            monitor.verify_invariants(&self.containers);

            for workspace in monitor.workspaces(&self.containers) {
                let preferred = workspace.find_preferred_output(
                    &self.containers,
                    monitors.iter().map(|monitor| &monitor.output),
                );
                let Some(preferred_idx) = preferred else {
                    assert_eq!(
                        idx, primary_idx,
                        "workspace without a connected output candidate must be on the primary \
                         monitor"
                    );
                    continue;
                };
                assert_eq!(
                    idx, preferred_idx,
                    "workspace must be on its preferred connected output"
                );
            }

            for workspace in monitor.workspaces(&self.containers) {
                assert!(
                    seen_workspace_id.insert(workspace.id()),
                    "workspace id must be unique"
                );

                if let Some(name) = workspace.name(&self.containers) {
                    assert!(
                        !seen_workspace_name
                            .iter()
                            .any(|n| n.eq_ignore_ascii_case(name)),
                        "workspace name must be unique"
                    );
                    seen_workspace_name.push(name.clone());
                }

                workspace.verify_invariants(&self.containers, move_win_id.as_ref());
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

        if let Some(InteractiveMoveState::Moving(move_)) = &self.interactive_move {
            self.containers
                .transferred_tile_mut(move_.node)
                .expect("moving view must remain transferred")
                .advance_animations();

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
            if self.monitor_for_output(&output).is_some() {
                let (scrolled, target) = {
                    let (mon, containers) = self
                        .monitor_for_output_and_containers_mut(&output)
                        .expect("monitor disappeared while processing DnD scroll");
                    let zoom = mon.overview_zoom();
                    let scrolled =
                        mon.dnd_scroll_gesture_scroll(&*containers, pos_within_output, 1. / zoom);
                    let target = (!scrolled && is_dnd).then(|| {
                        mon.window_under(containers, pos_within_output)
                            .map(|(win, _)| DndHoldTarget::Window(win.id().clone()))
                            .or_else(|| {
                                mon.workspace_under_narrow(&*containers, pos_within_output)
                                    .map(|ws| DndHoldTarget::Workspace(ws.id()))
                            })
                    });
                    (scrolled, target.flatten())
                };

                if scrolled {
                    // Don't trigger DnD hold while scrolling.
                    if let Some(dnd) = &mut self.dnd {
                        dnd.hold = None;
                    }
                } else if is_dnd {
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

                            if let Some((mon, containers)) =
                                self.monitor_for_output_and_containers_mut(&output)
                            {
                                let ws_idx = match hold.target {
                                    DndHoldTarget::Window(id) => {
                                        let idx = mon.workspace_idx_for_window(containers, &id);
                                        idx.filter(|&idx| {
                                            mon.ws(&*containers, idx)
                                                .activate_window(containers, &id)
                                        })
                                    }
                                    DndHoldTarget::Workspace(id) => {
                                        mon.workspaces(&*containers).position(|ws| ws.id() == id)
                                    }
                                };

                                if let Some(ws_idx) = ws_idx {
                                    mon.dnd_scroll_gesture_end(containers);
                                    mon.activate_workspace_with_anim_config(
                                        containers, ws_idx, config,
                                    );

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

        if let MonitorSet::Normal { monitors, .. } = &mut self.monitor_set {
            for mon in monitors {
                mon.set_overview_progress(&self.containers, self.overview_progress.as_ref());
                mon.advance_animations(&mut self.containers);
            }
        }
        for ws in self.workspace_handles() {
            ws.advance_animations(&mut self.containers);
        }

        self.scratchpad.advance_animations(&mut self.containers);
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
                if self
                    .containers
                    .transferred_tile(move_.node)
                    .expect("moving view must remain transferred")
                    .are_animations_ongoing()
                {
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

            if mon.are_animations_ongoing(&self.containers) {
                return true;
            }
        }

        false
    }

    pub fn update_render_elements(&mut self, output: Option<&Output>) {
        let _span = tracy_client::span!("Layout::update_render_elements");

        self.update_render_elements_time = self.clock.now();

        let zoom = self.overview_zoom();
        if let Some(InteractiveMoveState::Moving(move_)) = &self.interactive_move {
            if output.is_none_or(|output| move_.output == *output) {
                let tile = self
                    .containers
                    .transferred_tile_mut(move_.node)
                    .expect("moving view must remain transferred");
                let pos_within_output = move_.tile_render_location(tile, zoom);

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
                tile.update_render_elements(
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
                mon.set_overview_progress(&self.containers, self.overview_progress.as_ref());
                mon.update_render_elements(&mut self.containers, is_active);
            }
        }

        // Never active: nothing in the scratchpad is on screen. It still needs the pass, which
        // is where a branch's arrange is committed once its windows have answered.
        self.scratchpad
            .update_render_elements(&mut self.containers, false, RenderLayer::Normal);
    }

    pub fn update_shaders(&mut self) {
        if let Some(InteractiveMoveState::Moving(move_)) = &self.interactive_move {
            self.containers
                .transferred_tile_mut(move_.node)
                .expect("moving view must remain transferred")
                .update_shaders();
        }

        for mon in self.monitors_mut() {
            mon.update_shaders();
        }
        for ws in self.workspace_handles() {
            ws.update_shaders(&mut self.containers);
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

        if let Some((mon, containers)) = self.monitor_for_output_and_containers_mut(&move_.output) {
            let zoom = mon.overview_zoom();
            let (insert_ws, geo) =
                mon.insert_position(&*containers, move_.pointer_pos_within_output);
            match insert_ws {
                InsertWorkspace::Existing(ws_id) => {
                    let idx = mon.idx_of_ws(&*containers, ws_id).unwrap();
                    let ws = mon.ws(&*containers, idx);
                    let pos_within_workspace =
                        (move_.pointer_pos_within_output - geo.loc).downscale(zoom);
                    let position = if move_.is_floating {
                        InsertPosition::Floating
                    } else {
                        ws.tiling_insert_position(containers, pos_within_workspace)
                    };

                    let tile = containers
                        .transferred_tile(move_.node)
                        .expect("moving view must remain transferred");
                    let border_width = tile.effective_border_width().unwrap_or(0.);
                    let corner_radius = tile
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
                        InsertPosition::RootStart
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

        let options = self.options.clone();

        match &mut self.monitor_set {
            MonitorSet::Normal {
                monitors,
                primary_idx,
                active_monitor_idx,
            } => {
                let mon_idx = if ws_config.open_on_output.is_empty() {
                    *active_monitor_idx
                } else {
                    ws_config
                        .open_on_output
                        .iter()
                        .find_map(|name| {
                            monitors
                                .iter()
                                .position(|monitor| output_matches_name(&monitor.output, name))
                        })
                        .unwrap_or(*primary_idx)
                };
                let mon = &mut monitors[mon_idx];

                let ws = Workspace::new_with_config(
                    &mut self.containers,
                    mon.output.clone(),
                    Some(ws_config.clone()),
                    options,
                );
                let idx = mon.named_workspace_insert_idx(&self.containers);
                mon.insert_workspace(&mut self.containers, ws, idx, false);
            }
            MonitorSet::NoOutputs => {
                let ws = Workspace::new_with_config_no_outputs(
                    &mut self.containers,
                    Some(ws_config.clone()),
                    options,
                );
                self.push_outputless_workspace(ws);
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
        for ws in self.workspace_handles() {
            let containers = &mut self.containers;
            let Some(name) = ws.name(containers) else {
                continue;
            };
            if let Some(config) = config.workspaces.iter().find(|w| &w.name.0 == name) {
                ws.update_layout_config(containers, config.layout.clone().map(|x| x.0));
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
            self.containers
                .transferred_tile_mut(move_.node)
                .expect("moving view must remain transferred")
                .update_config(view_size, scale, Rc::new(options));
        }

        match &mut self.monitor_set {
            MonitorSet::Normal { monitors, .. } => {
                for mon in monitors {
                    mon.update_config(&mut self.containers, options.clone());
                }
            }
            MonitorSet::NoOutputs => {
                let workspaces = self.outputless_workspaces();
                for ws in workspaces {
                    ws.update_config(&mut self.containers, options.clone());
                }
            }
        }

        self.options = options;
        self.seat_focus_after_mutation();
    }

    /// Run a `split` or `layout` command on what the active workspace's commands are aimed at.
    pub fn layout_command(&mut self, command: LayoutCommand) {
        if let Some(workspace) = self.active_workspace().map(|ws| ws.workspace_key()) {
            self.containers.apply_layout_command(workspace, command);
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

    #[cfg(test)]
    /// Whether autotiling is currently on.
    pub fn is_autotile(&self) -> bool {
        self.options.layout.autotile
    }

    /// Run a size command on `window`, or on what the active workspace's commands are aimed
    /// at.
    pub fn size_command(&mut self, window: Option<&W::Id>, command: SizeCommand) {
        self.request_refresh();
        if let Some(InteractiveMoveState::Moving(move_)) = &self.interactive_move {
            if window.is_none() || window == Some(&move_.window_id) {
                return;
            }
        }

        let workspace = if let Some(window) = window {
            self.workspace_for_window_and_containers_mut(window)
        } else {
            self.active_workspace_and_containers_mut()
        };
        let Some((workspace, containers)) = workspace else {
            return;
        };
        workspace.size_command(containers, window, command);
    }
    pub fn toggle_window_floating(&mut self, window: Option<&W::Id>) {
        self.request_refresh();
        if let Some(InteractiveMoveState::Moving(move_)) = &mut self.interactive_move {
            if window.is_none() || window == Some(&move_.window_id) {
                move_.is_floating = !move_.is_floating;
                let tile = self
                    .containers
                    .transferred_tile_mut(move_.node)
                    .expect("moving view must remain transferred");

                // When going to floating, restore the floating window size.
                if move_.is_floating {
                    let floating_size = tile.floating_window_size;
                    let win = tile.window_mut();
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
                    tile.animate_alpha(
                        INTERACTIVE_MOVE_ALPHA,
                        1.,
                        self.options.animations.window_movement.0,
                    );
                } else {
                    // Animate the tile back to semitransparent.
                    tile.animate_alpha(
                        1.,
                        INTERACTIVE_MOVE_ALPHA,
                        self.options.animations.window_movement.0,
                    );
                    tile.hold_alpha_animation_after_done();
                }

                return;
            }
        }

        let target_workspace_id =
            window.and_then(|id| self.workspace_for_window(id).map(|ws| ws.id()));
        let target_window_was_workspace_focus = match (target_workspace_id, window) {
            (Some(workspace_id), Some(window_id)) => {
                self.seat_focus_workspace_targets_window(workspace_id, window_id)
            }
            _ => false,
        };
        let workspace = if let Some(window) = window {
            self.workspace_for_window_and_containers_mut(window)
        } else {
            self.active_workspace_and_containers_mut()
        };

        let Some((workspace, containers)) = workspace else {
            return;
        };
        workspace.toggle_window_floating(containers, window);
        if target_window_was_workspace_focus {
            if let Some(window_id) = window {
                workspace.activate_window(containers, window_id);
            }
        }
        self.seat_focus_after_mutation();
        if let Some(workspace_id) = target_workspace_id {
            self.seat_focus_record_workspace_chain(workspace_id);
        }
    }

    /// sway's `sticky toggle`, on `window`'s container or on what commands are aimed at.
    ///
    /// The flag goes on whatever container is addressed, tiled ones included; it only takes
    /// effect while that container is a floating root. A container that becomes sticky on a
    /// workspace its output is not showing moves to the one it is.
    ///
    /// sway/commands/sticky.c
    pub fn toggle_window_sticky(&mut self, window: Option<&W::Id>) {
        self.request_refresh();
        if let Some(InteractiveMoveState::Moving(move_)) = &self.interactive_move {
            if window.is_none() || window == Some(&move_.window_id) {
                return;
            }
        }

        let key = match window {
            Some(id) => self.containers.window_key(id),
            None => self
                .active_workspace()
                .and_then(|ws| self.containers.workspace_command_node(ws.workspace_key())),
        };
        let Some(key) = key else {
            return;
        };
        let sticky = !self.containers.sticky_flag(key);
        assert!(self.containers.set_sticky_flag(key, sticky));
        if !self.containers.is_sticky_or_child(key) {
            return;
        }

        let Some((monitor_idx, workspace_idx)) = self
            .containers
            .workspace_of(key)
            .and_then(|workspace| self.monitor_location(workspace))
        else {
            return;
        };
        let root = self.containers.branch_root(key);
        let MonitorSet::Normal { monitors, .. } = &mut self.monitor_set else {
            return;
        };
        monitors[monitor_idx].bring_float_to_active_workspace(
            &mut self.containers,
            workspace_idx,
            root,
        );
        self.seat_focus_after_mutation();
    }

    pub fn set_window_floating(&mut self, window: Option<&W::Id>, floating: bool) {
        self.request_refresh();
        if let Some(InteractiveMoveState::Moving(move_)) = &mut self.interactive_move {
            if window.is_none() || window == Some(&move_.window_id) {
                if move_.is_floating != floating {
                    self.toggle_window_floating(window);
                }
                return;
            }
        }

        let target_workspace_id =
            window.and_then(|id| self.workspace_for_window(id).map(|ws| ws.id()));
        let target_window_was_workspace_focus = match (target_workspace_id, window) {
            (Some(workspace_id), Some(window_id)) => {
                self.seat_focus_workspace_targets_window(workspace_id, window_id)
            }
            _ => false,
        };
        let workspace = if let Some(window) = window {
            self.workspace_for_window_and_containers_mut(window)
        } else {
            self.active_workspace_and_containers_mut()
        };

        let Some((workspace, containers)) = workspace else {
            return;
        };
        workspace.set_window_floating(containers, window, floating);
        if target_window_was_workspace_focus {
            if let Some(window_id) = window {
                workspace.activate_window(containers, window_id);
            }
        }
        self.seat_focus_after_mutation();
        if let Some(workspace_id) = target_workspace_id {
            self.seat_focus_record_workspace_chain(workspace_id);
        }
    }

    pub fn move_window_to_scratchpad(&mut self, window: Option<&W::Id>) {
        if let Some(InteractiveMoveState::Moving(move_)) = &self.interactive_move {
            if window.is_none() || window == Some(&move_.window_id) {
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

        let Some(workspace) = self.workspace_for_window(&target) else {
            return;
        };
        let tile = workspace
            .take_tile_for_hiding_in_scratchpad(&mut self.containers, &target)
            .expect("the resolved source workspace must still own the window");
        self.scratchpad
            .hide_in_scratchpad(&mut self.containers, tile.into_incoming());
        self.workspace_lost_window(workspace);
        self.seat_focus_after_mutation();
    }

    pub fn scratchpad_show(&mut self) {
        let (active_ws_id, active_visible) = {
            let Some(workspace) = self.active_workspace() else {
                return;
            };
            let id = workspace.id();
            let visible = workspace
                .scratchpad_window_id(&self.containers)
                .map(|visible_id| (id, visible_id));
            (id, visible)
        };

        let visible_elsewhere = active_visible.or_else(|| {
            self.workspaces().find_map(|(_, _, ws)| {
                if ws.id() == active_ws_id {
                    return None;
                }
                let id = ws.scratchpad_window_id(&self.containers)?;
                Some((ws.id(), id))
            })
        });

        // Showing one that is already out puts it away again; showing one that is out on
        // another workspace brings it here; otherwise the next one hidden comes out. Three
        // moves between workspaces, which is all sway's scratchpad ever does.
        if let Some((ws_id, visible_id)) = visible_elsewhere {
            if ws_id == active_ws_id {
                let tile = self.active_workspace_and_containers_mut().and_then(
                    |(workspace, containers)| {
                        workspace.take_tile_for_hiding_in_scratchpad(containers, &visible_id)
                    },
                );
                if let Some(tile) = tile {
                    self.scratchpad
                        .hide_in_scratchpad(&mut self.containers, tile.into_incoming());
                    self.seat_focus_after_mutation();
                }
                return;
            }

            let source_monitor_idx = self
                .find_workspace_location_by_id(ws_id)
                .map(|(idx, _)| idx);

            let tile = self
                .workspace_by_id_and_containers_mut(ws_id)
                .and_then(|(ws, containers)| ws.take_tile_for_scratchpad(containers, &visible_id));

            if let (Some(monitor_idx), MonitorSet::Normal { monitors, .. }) =
                (source_monitor_idx, &mut self.monitor_set)
            {
                if monitors[monitor_idx].workspace_switch.is_none() {
                    monitors[monitor_idx].clean_up_workspaces(&mut self.containers);
                }
            }

            if let (Some(tile), Some((monitor, containers))) =
                (tile, self.active_monitor_and_containers_mut())
            {
                monitor.add_scratchpad_tile(containers, tile.into_incoming(), true);
            }
            self.seat_focus_after_mutation();
            return;
        }

        let Some(next) = self.scratchpad.next_scratchpad_window(&self.containers) else {
            return;
        };
        let tile = self
            .scratchpad
            .take_tile_for_scratchpad(&mut self.containers, &next);
        if let (Some(tile), Some((monitor, containers))) =
            (tile, self.active_monitor_and_containers_mut())
        {
            monitor.add_scratchpad_tile(containers, tile.into_incoming(), true);
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
        self.containers.window_id_with_mark(mark)
    }

    /// sway's `swap container with mark <mark>`.
    pub fn swap_window_with_mark(&mut self, mark: &str) -> bool {
        let Some(source_workspace) = self.active_workspace().map(|ws| ws.workspace_key()) else {
            return false;
        };
        self.containers
            .swap_selected_with_mark(source_workspace, mark)
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
            if id.is_none() || id == Some(&move_.window_id) {
                return;
            }
        }

        let workspace = if let Some(id) = id {
            self.workspace_for_window_and_containers_mut(id)
        } else {
            self.active_workspace_and_containers_mut()
        };

        let Some((workspace, containers)) = workspace else {
            return;
        };
        workspace.move_floating_window(containers, id, x, y, animate);
    }

    pub fn focus_output(&mut self, output: &Output) {
        let target_monitor_idx = match &self.monitor_set {
            MonitorSet::Normal { monitors, .. } => {
                monitors.iter().position(|mon| &mon.output == output)
            }
            MonitorSet::NoOutputs => None,
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

            let ws_idx = monitors[idx].active_workspace_idx(&self.containers);
            let ws = monitors[idx].ws(&self.containers, ws_idx);
            target_has_tiling = !self.containers.tiled_is_empty(ws.workspace_key());
            if !target_has_tiling {
                ws.focus_workspace_node(&mut self.containers);
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
                        let ws = monitors[monitor_idx].ws(&self.containers, workspace_idx);
                        restored_tiling = ws
                            .restore_inactive_tiling(&mut self.containers)
                            .unwrap_or(false);
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
                            let ws = monitors[monitor_idx].ws(&self.containers, workspace_idx);
                            focused_by_edge_target = ws
                                .focus_entry_from_output_direction(&mut self.containers, direction);
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
                            let ws = monitors[monitor_idx].ws(&self.containers, workspace_idx);
                            ws.focus_tiling(&mut self.containers);
                        }
                    }
                }
            }
        }
    }

    fn move_to_output(
        &mut self,
        window: Option<&W::Id>,
        output: &Output,
        target_ws_idx: Option<usize>,
        activate: ActivateWindow,
    ) {
        self.request_refresh();
        if let Some(InteractiveMoveState::Moving(move_)) = &mut self.interactive_move {
            if window.is_none() || window == Some(&move_.window_id) {
                return;
            }
        }

        // Resolved here rather than where it is read, because reading it needs `&self` and the
        // block below holds `&mut self.monitor_set`. It is a position, so it is only true until
        // the monitor or workspace lists change, and nothing before its use does more than look.
        let regular_source = window.and_then(|id| self.monitor_workspace_indices_for_window(id));

        if let MonitorSet::Normal {
            monitors,
            active_monitor_idx,
            ..
        } = &mut self.monitor_set
        {
            let Some(new_idx) = monitors.iter().position(|mon| &mon.output == output) else {
                return;
            };

            let (mon_idx, ws_idx) = if window.is_some() {
                let Some(found) = regular_source else {
                    return;
                };
                found
            } else {
                let mon_idx = *active_monitor_idx;
                let mon = &monitors[mon_idx];
                (mon_idx, mon.active_workspace_idx(&self.containers))
            };

            let workspace_idx =
                target_ws_idx.unwrap_or(monitors[new_idx].active_workspace_idx(&self.containers));
            if mon_idx == new_idx && ws_idx == workspace_idx {
                return;
            }

            let mon = &monitors[new_idx];
            if mon.workspace_count(&self.containers) <= workspace_idx {
                return;
            }

            let ws_id = mon.ws(&self.containers, workspace_idx).id();

            let mon = &mut monitors[mon_idx];
            let activate = activate.map_smart(|| {
                window.is_none_or(|win| {
                    mon_idx == *active_monitor_idx
                        && mon.active_window(&self.containers).map(|win| win.id()) == Some(win)
                })
            });
            let activate = if activate {
                ActivateWindow::Yes
            } else {
                ActivateWindow::No
            };

            let ws = mon.ws(&self.containers, ws_idx);
            let Some(window) =
                window.or_else(|| ws.active_window(&self.containers).map(|win| win.id()))
            else {
                return;
            };
            let window = window.clone();

            // A sticky float is on whichever workspace its output shows, so another workspace
            // of the same output is not somewhere it can be put. sway refuses the command.
            //
            // sway/commands/move.c:542-545
            if mon_idx == new_idx
                && self
                    .containers
                    .window_key(&window)
                    .is_some_and(|key| self.containers.is_sticky_or_child(key))
            {
                return;
            }

            let transaction = Transaction::new();
            let transferred = ws.take_tile_for_transfer(&mut self.containers, &window, transaction);
            transferred.prepare_for_workspace_move(&mut self.containers);
            transferred
                .tile_mut(&mut self.containers)
                .stop_move_animations();
            let is_floating = transferred.is_floating;

            let mon = &mut monitors[new_idx];
            mon.add_tile(
                &mut self.containers,
                transferred.into_incoming(),
                MonitorAddWindowTarget::Workspace {
                    id: ws_id,
                    root_index: None,
                },
                activate,
                true,
                is_floating,
            );
            if activate.map_smart(|| false) {
                *active_monitor_idx = new_idx;
            }

            let mon = &mut monitors[mon_idx];
            if mon.workspace_switch.is_none() {
                monitors[mon_idx].clean_up_workspaces(&mut self.containers);
            }
        }
        self.seat_focus_after_mutation();
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
            let ws = current.active_workspace(&self.containers);

            if ws.floating_is_active(&self.containers) {
                self.move_to_output(None, output, None, ActivateWindow::Smart);
                return;
            }

            let subtree = if root_child {
                ws.remove_active_root_tiling_subtree(&mut self.containers)
            } else {
                ws.remove_active_tiling_subtree(&mut self.containers)
            };
            let Some(mut subtree) = subtree else {
                return;
            };
            subtree.prepare_for_workspace_move(&mut self.containers);

            let workspace_idx = target_ws_idx
                .unwrap_or(monitors[new_idx].active_workspace_idx(&self.containers))
                .min(monitors[new_idx].workspace_count(&self.containers) - 1);
            self.add_root_tiling_subtree_by_idx(new_idx, workspace_idx, subtree, activate);
        }
        self.seat_focus_after_mutation();
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

        let idx = monitors[*active_monitor_idx].active_workspace_idx(&self.containers);
        self.move_workspace_to_output_by_index(idx, None, output)
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
            mon.workspaces(&self.containers)
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

        if current.workspace_count(&self.containers) <= old_idx {
            return false;
        }

        // Do not do anything if the output is already correct
        if current_idx == target_idx {
            // Just update the original output since this is an explicit movement action.
            current
                .ws(&self.containers, old_idx)
                .set_original_output(&mut self.containers, &current.output);

            return false;
        }

        // Only switch active monitor if the workspace to be moved is the currently focused one on
        // the current monitor.
        let activate = current_idx == *active_monitor_idx
            && old_idx == current.active_workspace_idx(&self.containers);

        let ws = current.remove_workspace_by_idx(&mut self.containers, old_idx);
        ws.set_original_output(&mut self.containers, new_output);

        let target = &mut monitors[target_idx];
        let idx = target.active_workspace_idx(&self.containers) + 1;
        target.insert_workspace(&mut self.containers, ws, idx, activate);

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

        if let Some((ws, containers)) = self.workspace_for_window_and_containers_mut(id) {
            ws.set_fullscreen(containers, id, is_fullscreen);
        }
    }

    pub fn toggle_fullscreen(&mut self, id: &W::Id) {
        self.request_refresh();
        if self.interactive_move_targets_window(id) {
            return;
        }

        if let Some((ws, containers)) = self.workspace_for_window_and_containers_mut(id) {
            ws.toggle_fullscreen(containers, id);
        }
    }

    pub fn toggle_fullscreen_for_active_command(&mut self, id: &W::Id) {
        self.request_refresh();
        if self.interactive_move_targets_window(id) {
            return;
        }

        if let Some((ws, containers)) = self.workspace_for_window_and_containers_mut(id) {
            ws.toggle_fullscreen_for_command(containers, id);
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
            if self.window_is_floating(id) {
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

    fn interactive_move_targets_window(&self, id: &W::Id) -> bool {
        match &self.interactive_move {
            Some(InteractiveMoveState::Starting { window_id, .. }) => window_id == id,
            Some(InteractiveMoveState::Moving(move_)) => &move_.window_id == id,
            Some(InteractiveMoveState::MovingContainer(move_)) => &move_.window_id == id,
            None => false,
        }
    }

    pub fn workspace_switch_gesture_begin(&mut self, output: &Output, is_touchpad: bool) {
        let monitors = match &mut self.monitor_set {
            MonitorSet::Normal { monitors, .. } => monitors,
            MonitorSet::NoOutputs => unreachable!(),
        };

        for monitor in monitors {
            // Cancel the gesture on other outputs.
            if &monitor.output != output {
                monitor.workspace_switch_gesture_end(&mut self.containers, None);
                continue;
            }

            monitor.workspace_switch_gesture_begin(&self.containers, is_touchpad);
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
            MonitorSet::NoOutputs => return None,
        };

        for monitor in monitors {
            if let Some(refresh) = monitor.workspace_switch_gesture_update(
                &self.containers,
                delta_y,
                timestamp,
                is_touchpad,
            ) {
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
            MonitorSet::NoOutputs => return None,
        };

        for monitor in monitors {
            if monitor.workspace_switch_gesture_end(&mut self.containers, is_touchpad) {
                return Some(monitor.output.clone());
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

    pub fn interactive_resize_begin(&mut self, window: W::Id, edges: ResizeEdge) -> bool {
        self.request_refresh();
        let Some(workspace) = self.workspace_for_window(&window) else {
            return false;
        };
        workspace.interactive_resize_begin(&mut self.containers, window, edges)
    }

    pub fn interactive_resize_begin_at(
        &mut self,
        window: W::Id,
        edges: ResizeEdge,
        output: &Output,
        pos_within_output: Point<f64, Logical>,
    ) -> bool {
        self.request_refresh();
        let Some(workspace) = self.workspace_for_window(&window) else {
            return false;
        };
        let pos_within_workspace = match workspace.current_output(&self.containers) {
            Some(on) if on != output => return false,
            Some(_) => {
                let Some(loc) = self.workspace_render_loc(workspace) else {
                    return false;
                };
                pos_within_output - loc
            }
            None => pos_within_output,
        };
        workspace.interactive_resize_begin_at(
            &mut self.containers,
            window,
            edges,
            pos_within_workspace,
        )
    }

    pub fn interactive_resize_update(
        &mut self,
        window: &W::Id,
        delta: Point<f64, Logical>,
    ) -> bool {
        self.request_refresh();
        if let Some(InteractiveMoveState::Moving(move_)) = &self.interactive_move {
            if &move_.window_id == window {
                return false;
            }
        }

        if let Some((ws, containers)) = self.workspace_for_window_and_containers_mut(window) {
            return ws.interactive_resize_update(containers, window, delta);
        }

        false
    }

    pub fn interactive_resize_end(&mut self, window: &W::Id) {
        self.request_refresh();
        if let Some(InteractiveMoveState::Moving(move_)) = &self.interactive_move {
            if &move_.window_id == window {
                return;
            }
        }

        if let Some((ws, containers)) = self.workspace_for_window_and_containers_mut(window) {
            ws.interactive_resize_end(containers, Some(window));
        }
    }

    pub fn move_workspace_down(&mut self) {
        let Some((monitor, containers)) = self.active_monitor_and_containers_mut() else {
            return;
        };
        monitor.move_workspace_down(containers);
        self.seat_focus_after_mutation();
    }

    pub fn move_workspace_up(&mut self) {
        let Some((monitor, containers)) = self.active_monitor_and_containers_mut() else {
            return;
        };
        monitor.move_workspace_up(containers);
        self.seat_focus_after_mutation();
    }

    pub fn move_workspace_to_idx(
        &mut self,
        reference: Option<(Option<Output>, usize)>,
        new_idx: usize,
    ) {
        let (monitor, containers, old_idx) = if let Some((output, old_idx)) = reference {
            let monitor = if let Some(output) = output {
                let Some(monitor) = self.monitor_for_output_and_containers_mut(&output) else {
                    return;
                };
                monitor
            } else {
                // In case a numbered workspace reference is used, assume the active monitor
                let Some(monitor) = self.active_monitor_and_containers_mut() else {
                    return;
                };
                monitor
            };

            (monitor.0, monitor.1, old_idx)
        } else {
            let Some((monitor, containers)) = self.active_monitor_and_containers_mut() else {
                return;
            };
            let index = monitor.active_workspace_idx(&*containers);
            (monitor, containers, index)
        };

        monitor.move_workspace_to_idx(containers, old_idx, new_idx);
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
                        mon.workspaces(&self.containers)
                            .position(|ws| ws.id() == workspace_id)
                            .map(|idx| (mon_idx, idx))
                    })
                else {
                    return;
                };

                monitors[mon_idx].move_workspace_to_idx(&mut self.containers, old_idx, new_idx);
            }
            MonitorSet::NoOutputs => {
                let workspaces = self.outputless_workspaces();
                let Some(old_idx) = workspaces.iter().position(|ws| ws.id() == workspace_id) else {
                    return;
                };

                let new_idx = new_idx.min(workspaces.len() - 1);
                self.containers
                    .reorder_output_workspaces(self.fallback_output, |workspaces| {
                        if old_idx < new_idx {
                            workspaces[old_idx..=new_idx].rotate_left(1);
                        } else {
                            workspaces[new_idx..=old_idx].rotate_right(1);
                        }
                    });
            }
        }
        self.seat_focus_after_mutation();
    }

    pub fn set_workspace_name(&mut self, name: String, reference: Option<WorkspaceReference>) {
        // ignore the request if the name is already used by another workspace
        if self.find_workspace_by_name(&name).is_some() {
            return;
        }

        let Some(key) = self.workspace_key_by_ref(reference) else {
            return;
        };
        self.containers
            .workspace_properties_mut(key)
            .set_name(name, WorkspaceLifetime::Persistent);
        let wsid = WorkspaceId::of(key);

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
                    .workspaces(&self.containers)
                    .next()
                    .is_some_and(|first| first.id() == wsid)
            {
                monitor.add_workspace_top(&mut self.containers);
            }
            if monitor
                .workspaces(&self.containers)
                .last()
                .is_some_and(|last| last.id() == wsid)
            {
                monitor.add_workspace_bottom(&mut self.containers);
            }
        }
    }

    pub fn unset_workspace_name(&mut self, reference: Option<WorkspaceReference>) {
        if let Some(key) = self.workspace_key_by_ref(reference) {
            self.unname_workspace_by_id(WorkspaceId::of(key));
        }
    }

    pub fn set_monitors_overview_state(&mut self) {
        let MonitorSet::Normal { monitors, .. } = &mut self.monitor_set else {
            return;
        };

        for mon in monitors {
            mon.overview_open = self.overview_open;
            mon.set_overview_progress(&self.containers, self.overview_progress.as_ref());
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
        if let Some((mon, containers)) = self.active_monitor_and_containers_mut() {
            mon.activate_workspace_with_anim_config(containers, ws_idx, Some(config));
        }
        self.toggle_overview();
    }

    pub fn start_open_animation_for_window(&mut self, window: &W::Id) {
        if let Some(InteractiveMoveState::Moving(move_)) = &self.interactive_move {
            if &move_.window_id == window {
                return;
            }
        }

        if let Some((ws, containers)) = self.workspace_for_window_and_containers_mut(window) {
            let _ = ws.start_open_animation(containers, window);
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

        if let Some(InteractiveMoveState::Moving(move_)) = &self.interactive_move {
            if &move_.window_id == window {
                let tile = self
                    .containers
                    .transferred_tile_mut(move_.node)
                    .expect("moving view must remain transferred");
                let pos_within_output = move_.tile_render_location(tile, zoom);

                // Computation matches update_render_elements().
                let view_rect =
                    Rectangle::new(pos_within_output.upscale(-1.), output_size(&move_.output))
                        .downscale(zoom);
                tile.update_render_elements(
                    false,
                    false,
                    false,
                    crate::layout::focus_ring::FocusRingEdges::all(),
                    None,
                    view_rect,
                );

                tile.store_unmap_snapshot_if_empty(
                    renderer,
                    xray,
                    xray_has_blocked_out_layers,
                    XrayPos::new(pos_within_output, zoom),
                );
                return;
            }
        }

        let Some(workspace) = self.workspace_for_window(window) else {
            return;
        };
        let xray_pos = if workspace.current_output(&self.containers).is_some() {
            let Some(loc) = self.workspace_render_loc(workspace) else {
                return;
            };
            XrayPos::new(loc, zoom)
        } else {
            XrayPos::default()
        };
        workspace.store_unmap_snapshot_if_empty(
            &mut self.containers,
            renderer,
            xray,
            xray_has_blocked_out_layers,
            xray_pos,
            window,
        );
    }

    pub fn clear_unmap_snapshot(&mut self, window: &W::Id) {
        if let Some(InteractiveMoveState::Moving(move_)) = &self.interactive_move {
            if &move_.window_id == window {
                let _ = self
                    .containers
                    .transferred_tile_mut(move_.node)
                    .expect("moving view must remain transferred")
                    .take_unmap_snapshot();
                return;
            }
        }

        if let Some(workspace) = self.workspace_for_window(window) {
            workspace.clear_unmap_snapshot(&mut self.containers, window);
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

        if let Some(InteractiveMoveState::Moving(move_)) = &self.interactive_move {
            if &move_.window_id == window {
                let tile = self
                    .containers
                    .transferred_tile_mut(move_.node)
                    .expect("moving view must remain transferred");
                let Some(snapshot) = tile.take_unmap_snapshot() else {
                    return;
                };
                let tile_pos = move_.tile_render_location(tile, zoom);
                let tile_size = tile.tile_size();

                let output = move_.output.clone();
                let pointer_pos_within_output = move_.pointer_pos_within_output;
                let Some((mon, containers)) = self.monitor_for_output_and_containers_mut(&output)
                else {
                    return;
                };
                let Some((ws, ws_geo)) =
                    mon.workspace_under(&*containers, pointer_pos_within_output)
                else {
                    return;
                };
                let idx = mon.idx_of_ws(&*containers, ws.id()).unwrap();
                let ws = mon.ws(&*containers, idx);

                let tile_pos = tile_pos - ws_geo.loc;
                ws.start_close_animation_for_tile(
                    containers, renderer, snapshot, tile_size, tile_pos, blocker,
                );
                return;
            }
        }

        if let Some(workspace) = self.workspace_for_window(window) {
            workspace.start_close_animation_for_window(
                &mut self.containers,
                renderer,
                window,
                blocker,
            );
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
        let tile = self
            .containers
            .transferred_tile(move_.node)
            .expect("moving view must remain transferred");
        let pos_in_backdrop = move_.tile_render_location(tile, zoom);
        let xray_pos = XrayPos::new(pos_in_backdrop, zoom);

        tile.render(ctx, pos_in_backdrop, xray_pos, true, &mut |elem| {
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

        if let Some(InteractiveMoveState::Moving(move_)) = &self.interactive_move {
            let win = self
                .containers
                .transferred_tile_mut(move_.node)
                .expect("moving view must remain transferred")
                .window_mut();

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
                self.workspace_for_window(window_id)
                    .is_some_and(|_| !self.window_is_floating(window_id))
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
                        mon.dnd_scroll_gesture_begin(&self.containers);
                    } else if !self.overview_open {
                        mon.dnd_scroll_gesture_end(&mut self.containers);
                    }

                    for (ws_idx, ws) in mon.workspace_list(&self.containers).into_iter().enumerate()
                    {
                        let is_focused =
                            is_active && ws_idx == mon.active_workspace_idx(&self.containers);
                        ws.refresh(&mut self.containers, is_active, is_focused);
                    }
                }
            }
            MonitorSet::NoOutputs => {
                let workspaces = self.outputless_workspaces();
                for ws in workspaces {
                    ws.refresh(&mut self.containers, false, false);
                }
            }
        }

        // The scratchpad is a workspace and is refreshed like one. This is what keeps a
        // hidden window in step with its client: its configures go out and are acked while it
        // is away, so bringing it back is a move rather than a negotiation.
        self.scratchpad.refresh(&mut self.containers, false, false);
    }

    pub fn are_window_resize_animations_enabled(&self) -> bool {
        !self.options.animations.off && !self.options.animations.window_resize.anim.off
    }

    pub fn workspaces(
        &self,
    ) -> impl Iterator<Item = (Option<&Monitor<W>>, usize, Workspace<W>)> + '_ {
        let (iter_normal, iter_no_outputs) = match &self.monitor_set {
            MonitorSet::Normal { monitors, .. } => {
                let it = monitors.iter().flat_map(|mon| {
                    mon.workspaces(&self.containers)
                        .enumerate()
                        .map(move |(idx, ws)| (Some(mon), idx, ws))
                });

                (Some(it), None)
            }
            MonitorSet::NoOutputs => {
                let it = self
                    .containers
                    .output_workspaces(self.fallback_output)
                    .iter()
                    .map(|&key| Workspace::of(key))
                    .enumerate()
                    .map(|(idx, ws)| (None, idx, ws));

                (None, Some(it))
            }
        };

        let iter_normal = iter_normal.into_iter().flatten();
        let iter_no_outputs = iter_no_outputs.into_iter().flatten();
        iter_normal.chain(iter_no_outputs)
    }

    /// Every workspace on an output or waiting for one, collected, for a walk that changes the
    /// tree as it goes.
    fn workspace_handles(&self) -> Vec<Workspace<W>> {
        self.workspaces().map(|(_, _, ws)| ws).collect()
    }

    /// Every workspace with its monitor and index, paired with the tree they all live in.
    #[cfg(test)]
    #[allow(clippy::type_complexity)]
    pub(crate) fn workspaces_and_containers(
        &self,
    ) -> (
        impl Iterator<Item = (Option<&Monitor<W>>, usize, Workspace<W>)> + '_,
        &ContainerTree<W>,
    ) {
        (self.workspaces(), &self.containers)
    }

    /// Every workspace, paired with the tree they all live in.
    #[cfg(test)]
    pub(crate) fn workspaces_and_containers_mut(
        &mut self,
    ) -> (
        impl Iterator<Item = Workspace<W>> + use<W>,
        &mut ContainerTree<W>,
    ) {
        (self.workspace_handles().into_iter(), &mut self.containers)
    }

    pub fn windows(&self) -> impl Iterator<Item = (Option<&Monitor<W>>, &W)> {
        let moving_window = self
            .interactive_move
            .as_ref()
            .and_then(|x| x.moving())
            .and_then(|move_| {
                self.containers
                    .transferred_tile(move_.node)
                    .map(|tile| (self.monitor_for_output(&move_.output), tile.window()))
            })
            .into_iter();

        let rest = self.workspaces().flat_map(|(mon, _, ws)| {
            self.containers
                .windows_in_workspace(ws.workspace_key())
                .map(move |win| (mon, win))
        });

        let scratchpad = self
            .containers
            .tiles_in_workspace(self.scratchpad.workspace_key())
            .map(|tile| (None, tile.window()));

        moving_window.chain(rest).chain(scratchpad)
    }

    fn active_mark_target_key(&self) -> Option<NodeKey> {
        self.active_monitor_ref()?
            .active_mark_target_key(&self.containers)
    }

    fn node_has_mark(&self, key: NodeKey, mark: &str) -> bool {
        self.containers.node_has_mark(key, mark)
    }

    fn add_mark_to_node(&mut self, key: NodeKey, mark: String) {
        let _ = self.containers.add_mark_to_node(key, mark);
    }

    fn remove_mark_from_node(&mut self, key: NodeKey, mark: &str) {
        let _ = self.containers.remove_mark_from_node(key, mark);
    }

    fn clear_marks_on_node(&mut self, key: NodeKey) {
        let _ = self.containers.clear_marks_on_node(key);
    }

    fn clear_marks_everywhere(&mut self) {
        self.containers.clear_marks_everywhere();
    }

    fn remove_mark_everywhere(&mut self, mark: &str) {
        self.containers.remove_mark_everywhere(mark);
    }

    pub fn has_window(&self, window: &W::Id) -> bool {
        self.containers.window_key(window).is_some()
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
        let workspace = monitor.ws(
            &self.containers,
            monitor.active_workspace_idx(&self.containers),
        );

        LayoutTree {
            workspace_id: Some(workspace.id().get()),
            workspace_name: workspace.name(&self.containers).cloned(),
            output: Some(monitor.output.name()),
            root: workspace.layout_tree(&self.containers),
            floating: self
                .containers
                .floating_layout_tree_nodes(workspace.workspace_key()),
        }
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
