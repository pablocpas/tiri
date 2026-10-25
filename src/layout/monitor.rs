use std::cmp::min;
use std::iter::zip;
use std::marker::PhantomData;
use std::rc::Rc;
use std::time::Duration;

use smithay::backend::renderer::element::utils::{
    CropRenderElement, Relocate, RelocateRenderElement, RescaleRenderElement,
};
use smithay::output::Output;
use smithay::utils::{Logical, Point, Rectangle, Size};
use tiri_config::{CornerRadius, LayoutPart};

use super::container::{Direction, IncomingView, NodeKey, OutputKey};
use super::container_tree::{ContainerTree, RootTilingSubtree};
use super::floating::FloatingRoots;
use super::insert_hint_element::{InsertHintElement, InsertHintRenderElement};
use super::workspace::{
    compute_working_area, Workspace, WorkspaceAddWindowTarget, WorkspaceId, WorkspaceIdentity,
    WorkspaceLifetime, WorkspaceRenderElement,
};
use super::{
    compute_overview_zoom, ActivateWindow, HitType, LayoutElement, Options, TransferSubject,
    TransferTarget, WorkspaceTarget,
};
use crate::animation::{Animation, Clock};
use crate::input::swipe_tracker::SwipeTracker;
use crate::layout::RenderLayer;
use crate::niri_render_elements;
use crate::render_helpers::renderer::NiriRenderer;
use crate::render_helpers::shadow::ShadowRenderElement;
use crate::render_helpers::solid_color::SolidColorRenderElement;
use crate::render_helpers::xray::XrayPos;
use crate::render_helpers::RenderCtx;
use crate::rubber_band::RubberBand;
use crate::utils::transaction::Transaction;
use crate::utils::{output_size, round_logical_in_physical, round_logical_in_physical_max1};

/// Amount of touchpad movement to scroll the height of one workspace.
const WORKSPACE_GESTURE_MOVEMENT: f64 = 300.;

const WORKSPACE_GESTURE_RUBBER_BAND: RubberBand = RubberBand {
    stiffness: 0.5,
    limit: 0.05,
};

/// Amount of DnD edge scrolling to scroll the height of one workspace.
///
/// This constant is tied to the default dnd-edge-workspace-switch max-speed setting.
const WORKSPACE_DND_EDGE_SCROLL_MOVEMENT: f64 = 1500.;

#[derive(Debug)]
pub struct Monitor<W: LayoutElement> {
    /// Output for this monitor.
    pub(super) output: Output,
    /// Cached name of the output.
    output_name: String,
    /// Latest known working area for this output.
    ///
    /// Memoized where the scale and the view size are not: deriving it reads the output's
    /// layer map through a `RefCell`, which a caller that already holds it mutably would
    /// panic on. `update_output_size` and a struts change are the two places it is refreshed.
    ///
    /// Not rounded to physical pixels.
    // FIXME: since this is used for things like DnD scrolling edges in the overview, ideally this
    // should only consider overlay and top layer-shell surfaces. However, Smithay doesn't easily
    // let you do this at the moment.
    working_area: Rectangle<f64, Logical>,
    /// This output in the layout's tree, which holds the workspaces on it, in order.
    ///
    /// Always at least one.
    pub(super) output_key: OutputKey,
    /// The workspace this output shows.
    ///
    /// The workspace and not its index: the output's workspaces are inserted, removed and
    /// reordered all the time, and an index would have to be moved along by hand at every
    /// one of those, which is where it goes wrong.
    active_workspace: NodeKey,
    /// ID of the previously active workspace.
    pub(super) previous_workspace_id: Option<WorkspaceId>,
    /// In-progress switch between workspaces.
    pub(super) workspace_switch: Option<WorkspaceSwitch>,
    /// Indication where an interactively-moved window is about to be placed.
    pub(super) insert_hint: Option<InsertHint>,
    /// Insert hint element for rendering.
    insert_hint_element: InsertHintElement,
    /// Location to render the insert hint element.
    insert_hint_render_loc: Option<InsertHintRenderLoc>,
    /// Whether the overview is open.
    pub(super) overview_open: bool,
    /// Progress of the overview zoom animation, 1 is fully in overview.
    overview_progress: Option<OverviewProgress>,
    /// Clock for driving animations.
    pub(super) clock: Clock,
    /// Configurable properties of the layout as received from the parent layout.
    pub(super) base_options: Rc<Options>,
    /// Configurable properties of the layout.
    pub(super) options: Rc<Options>,
    /// Layout config overrides for this monitor.
    layout_config: Option<tiri_config::LayoutPart>,
    /// The workspace that holds this output's sticky floating containers.
    ///
    /// sway moves sticky floats from the workspace the output was showing to the one it shows
    /// next (`seat_set_workspace_focus`). Remembering which workspace that was, rather than
    /// reacting to each index write, makes the move depend on the active workspace changing
    /// identity and not on which of the many reorderings of the output's workspaces happened.
    sticky_host: WorkspaceId,
    /// The kind of window the tree this output belongs to holds.
    ///
    /// The monitor stores nothing of that kind any more (its workspaces are the tree's), but
    /// every method takes that tree, and keeping the type ties each monitor to trees of its
    /// own kind at compile time.
    _element: PhantomData<fn() -> W>,
}

#[derive(Debug)]
pub enum WorkspaceSwitch {
    Animation(Animation),
    Gesture(WorkspaceSwitchGesture),
}

#[derive(Debug)]
pub struct WorkspaceSwitchGesture {
    /// Index of the workspace where the gesture was started.
    center_idx: usize,
    /// Fractional workspace index where the gesture was started.
    ///
    /// Can differ from center_idx when starting a gesture in the middle between workspaces, for
    /// example by "catching" an animation.
    start_idx: f64,
    /// Current, fractional workspace index.
    pub(super) current_idx: f64,
    /// Animation for the extra offset to the current position.
    ///
    /// For example, if there's a workspace switch during a DnD scroll.
    animation: Option<Animation>,
    tracker: SwipeTracker,
    /// Whether the gesture is controlled by the touchpad.
    is_touchpad: bool,
    /// Whether the gesture is clamped to +-1 workspace around the center.
    is_clamped: bool,

    // If this gesture is for drag-and-drop scrolling, this is the last event's unadjusted
    // timestamp.
    dnd_last_event_time: Option<Duration>,
    // Time when the drag-and-drop scroll delta became non-zero, used for debouncing.
    //
    // If `None` then the scroll delta is currently zero.
    dnd_nonzero_start_time: Option<Duration>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) enum SplitIndicator {
    LayoutBorder,
    Center,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) enum InsertPosition {
    /// Before every existing child of the workspace's tiling root.
    RootStart,
    Swap {
        target: NodeKey,
        direction: Direction,
    },
    Split {
        target: NodeKey,
        direction: Direction,
        indicator: SplitIndicator,
    },
    SplitRoot {
        direction: Direction,
        indicator: SplitIndicator,
    },
    Floating,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum InsertWorkspace {
    Existing(WorkspaceId),
    NewAt(usize),
}

#[derive(Debug)]
pub(super) struct InsertHint {
    pub workspace: InsertWorkspace,
    pub position: InsertPosition,
    pub corner_radius: CornerRadius,
}

#[derive(Debug, Clone, Copy)]
struct InsertHintRenderLoc {
    workspace: InsertWorkspace,
    location: Point<f64, Logical>,
}

#[derive(Debug)]
pub(super) enum OverviewProgress {
    Animation(Animation),
    Value(f64),
}

/// Where to put a newly added window.
#[derive(Debug, Default, PartialEq, Eq)]
pub enum MonitorAddWindowTarget<'a, W: LayoutElement> {
    /// No particular preference.
    #[default]
    Auto,
    /// On this workspace.
    Workspace {
        /// Id of the target workspace.
        id: WorkspaceId,
        /// Override the index the window takes among the tiling root's children.
        root_index: Option<usize>,
    },
    /// Next to this existing window.
    NextTo(&'a W::Id),
}

impl<'a, W: LayoutElement> Copy for MonitorAddWindowTarget<'a, W> {}

impl<'a, W: LayoutElement> Clone for MonitorAddWindowTarget<'a, W> {
    fn clone(&self) -> Self {
        *self
    }
}

niri_render_elements! {
    MonitorInnerRenderElement<R> => {
        Workspace = CropRenderElement<WorkspaceRenderElement<R>>,
        InsertHint = CropRenderElement<InsertHintRenderElement>,
        UncroppedInsertHint = InsertHintRenderElement,
        Shadow = ShadowRenderElement,
        SolidColor = SolidColorRenderElement,
    }
}

pub type MonitorRenderElement<R> =
    RelocateRenderElement<RescaleRenderElement<MonitorInnerRenderElement<R>>>;

impl WorkspaceSwitch {
    pub fn current_idx(&self) -> f64 {
        match self {
            WorkspaceSwitch::Animation(anim) => anim.value(),
            WorkspaceSwitch::Gesture(gesture) => {
                gesture.current_idx + gesture.animation.as_ref().map_or(0., |anim| anim.value())
            }
        }
    }

    pub fn target_idx(&self) -> f64 {
        match self {
            WorkspaceSwitch::Animation(anim) => anim.to(),
            WorkspaceSwitch::Gesture(gesture) => gesture.current_idx,
        }
    }

    pub fn offset(&mut self, delta: isize) {
        match self {
            WorkspaceSwitch::Animation(anim) => anim.offset(delta as f64),
            WorkspaceSwitch::Gesture(gesture) => {
                if delta >= 0 {
                    gesture.center_idx += delta as usize;
                } else {
                    gesture.center_idx -= (-delta) as usize;
                }
                gesture.start_idx += delta as f64;
                gesture.current_idx += delta as f64;
            }
        }
    }

    fn is_animation_ongoing(&self) -> bool {
        match self {
            WorkspaceSwitch::Animation(_) => true,
            WorkspaceSwitch::Gesture(gesture) => gesture.animation.is_some(),
        }
    }
}

impl WorkspaceSwitchGesture {
    fn min_max(&self, workspace_count: usize) -> (f64, f64) {
        if self.is_clamped {
            let min = self.center_idx.saturating_sub(1) as f64;
            let max = (self.center_idx + 1).min(workspace_count - 1) as f64;
            (min, max)
        } else {
            (0., (workspace_count - 1) as f64)
        }
    }

    fn animate_from(&mut self, from: f64, clock: Clock, config: tiri_config::Animation) {
        let current = self.animation.as_ref().map_or(0., Animation::value);
        self.animation = Some(Animation::new(clock, from + current, 0., 0., config));
    }
}

impl InsertWorkspace {
    fn existing_id(self) -> Option<WorkspaceId> {
        match self {
            InsertWorkspace::Existing(id) => Some(id),
            InsertWorkspace::NewAt(_) => None,
        }
    }
}

impl OverviewProgress {
    pub fn value(&self) -> f64 {
        match self {
            OverviewProgress::Animation(anim) => anim.value(),
            OverviewProgress::Value(v) => *v,
        }
    }

    pub fn clamped_value(&self) -> f64 {
        match self {
            OverviewProgress::Animation(anim) => anim.clamped_value(),
            OverviewProgress::Value(v) => *v,
        }
    }
}

impl From<&super::OverviewProgress> for OverviewProgress {
    fn from(value: &super::OverviewProgress) -> Self {
        match value {
            super::OverviewProgress::Animation(anim) => Self::Animation(anim.clone()),
            super::OverviewProgress::Gesture(gesture) => Self::Value(gesture.value),
            super::OverviewProgress::Open => Self::Value(1.),
        }
    }
}

impl<W: LayoutElement> Monitor<W> {
    pub fn new(
        containers: &mut ContainerTree<W>,
        output: Output,
        workspaces: Vec<Workspace<W>>,
        ws_id_to_activate: Option<WorkspaceId>,
        clock: Clock,
        base_options: Rc<Options>,
        layout_config: Option<LayoutPart>,
    ) -> Self {
        let options =
            Rc::new(Options::clone(&base_options).with_merged_layout(layout_config.as_ref()));

        let scale = output.current_scale();
        let working_area =
            compute_working_area(&output, options.layout.struts, scale.fractional_scale());

        // Prepare the workspaces: set output, empty first, empty last.
        let output_key = containers.add_output(Some(output.clone()));
        let mut active_workspace = None;

        let had_workspaces = !workspaces.is_empty();
        for (idx, ws) in workspaces.into_iter().enumerate() {
            assert!(ws.has_windows_or_persistent_identity(containers));

            ws.attach(containers, output_key, idx);
            ws.update_config(containers, options.clone());

            if ws_id_to_activate.is_some_and(|id| ws.id() == id) {
                active_workspace = Some(ws.workspace_key());
            }
        }

        if options.layout.empty_workspace_above_first && had_workspaces {
            let ws = Workspace::new(containers, output.clone(), options.clone());
            ws.attach(containers, output_key, 0);
        }

        let ws = Workspace::new(containers, output.clone(), options.clone());
        let end = containers.output_workspaces(output_key).len();
        ws.attach(containers, output_key, end);
        // The first workspace, unless asked for another: with an empty one above the first,
        // that is the one after it.
        let first = usize::from(options.layout.empty_workspace_above_first && had_workspaces);
        let active_workspace =
            active_workspace.unwrap_or(containers.output_workspaces(output_key)[first]);
        let sticky_host = WorkspaceId::of(active_workspace);

        Self {
            output_name: output.name(),
            output,
            working_area,
            output_key,
            active_workspace,
            previous_workspace_id: None,
            insert_hint: None,
            insert_hint_element: InsertHintElement::new(options.layout.insert_hint),
            insert_hint_render_loc: None,
            overview_open: false,
            overview_progress: None,
            workspace_switch: None,
            clock,
            base_options,
            options,
            layout_config,
            sticky_host,
            _element: PhantomData,
        }
    }

    pub fn into_workspaces(self, containers: &mut ContainerTree<W>) -> Vec<Workspace<W>> {
        for ws in self.workspace_list(containers) {
            if !ws.has_windows_or_persistent_identity(containers) {
                ws.remove_from_tree(containers);
            }
        }

        let workspaces = self.workspace_list(containers);
        for ws in &workspaces {
            ws.detach(containers);
        }
        let left = containers.remove_output(self.output_key);
        debug_assert!(left.is_empty(), "every workspace has left the output");
        workspaces
    }

    /// The workspaces on this output, in order.
    pub fn workspace_keys<'a>(&self, containers: &'a ContainerTree<W>) -> &'a [NodeKey] {
        containers.output_workspaces(self.output_key)
    }

    /// The workspaces on this output, in order.
    pub fn workspaces<'a>(
        &self,
        containers: &'a ContainerTree<W>,
    ) -> impl DoubleEndedIterator<Item = Workspace<W>> + ExactSizeIterator + 'a {
        self.workspace_keys(containers)
            .iter()
            .map(|&key| Workspace::of(key))
    }

    /// The same, collected, for a walk that changes the tree as it goes.
    pub(super) fn workspace_list(&self, containers: &ContainerTree<W>) -> Vec<Workspace<W>> {
        self.workspaces(containers).collect()
    }

    /// The workspace at `idx` on this output.
    pub fn ws(&self, containers: &ContainerTree<W>, idx: usize) -> Workspace<W> {
        Workspace::of(self.workspace_keys(containers)[idx])
    }

    fn last_ws(&self, containers: &ContainerTree<W>) -> Workspace<W> {
        Workspace::of(*self.workspace_keys(containers).last().unwrap())
    }

    /// Take the workspace at `idx` off this output.
    fn detach_workspace_at(
        &mut self,
        containers: &mut ContainerTree<W>,
        idx: usize,
    ) -> Workspace<W> {
        let ws = self.ws(containers, idx);
        ws.detach(containers);
        ws
    }

    /// Take the workspace at `idx` off this output because it is going back to its own.
    ///
    /// If it was the one shown, the one above it is shown instead, so as not to end up on the
    /// last, empty, workspace. With an empty workspace above the first, the workspace at index
    /// 1 (the first real one) is replaced by the one after it instead, for the same reason:
    /// otherwise the first output to connect at startup, with named workspaces set up across
    /// several outputs, can end up showing the empty workspace at the top rather than the first
    /// named one.
    ///
    /// The sticky floats are left to the caller, which takes several workspaces off in a row.
    pub(super) fn release_workspace_at(
        &mut self,
        containers: &mut ContainerTree<W>,
        idx: usize,
    ) -> Workspace<W> {
        let ws = self.detach_workspace_at(containers, idx);
        if ws.workspace_key() == self.active_workspace {
            let replacement = if self.options.layout.empty_workspace_above_first && idx == 1 {
                idx
            } else {
                idx.saturating_sub(1)
            };
            let replacement = replacement.min(self.workspace_count(containers) - 1);
            self.active_workspace = self.ws(containers, replacement).workspace_key();
        }
        ws
    }

    /// Drop the workspace at `idx`, which must hold nothing worth keeping.
    fn remove_workspace_at(&mut self, containers: &mut ContainerTree<W>, idx: usize) {
        let ws = self.ws(containers, idx);
        assert!(containers.remove_workspace(ws.workspace_key()));
    }

    pub fn output(&self) -> &Output {
        &self.output
    }

    pub fn output_name(&self) -> &String {
        &self.output_name
    }

    pub fn active_workspace_idx(&self, containers: &ContainerTree<W>) -> usize {
        self.workspace_keys(containers)
            .iter()
            .position(|&key| key == self.active_workspace)
            .expect("the active workspace must be on its output")
    }

    pub fn workspace_count(&self, containers: &ContainerTree<W>) -> usize {
        self.workspace_keys(containers).len()
    }

    pub fn is_internal_empty_workspace(&self, containers: &ContainerTree<W>, idx: usize) -> bool {
        if idx >= self.workspace_count(containers) {
            return false;
        }

        // Keep the currently active empty workspace visible. It represents the "current"
        // workspace from the user's perspective.
        if idx == self.active_workspace_idx(containers) {
            return false;
        }

        let ws = self.ws(containers, idx);
        if ws.has_windows_or_persistent_identity(containers) {
            return false;
        }

        let has_other_real_workspaces = self.workspaces(containers).enumerate().any(|(i, ws)| {
            i != idx
                && (ws.has_windows_or_persistent_identity(containers)
                    || (i == self.active_workspace_idx(containers)
                        && ws.name(containers).is_some()))
        });
        if !has_other_real_workspaces {
            return false;
        }

        idx == self.workspace_count(containers) - 1
            || (self.options.layout.empty_workspace_above_first && idx == 0)
    }

    pub fn visible_workspace_idx(
        &self,
        containers: &ContainerTree<W>,
        ws_idx: usize,
    ) -> Option<usize> {
        if self.is_internal_empty_workspace(containers, ws_idx) {
            return None;
        }

        let mut visible = 0usize;
        for idx in 0..=ws_idx {
            if !self.is_internal_empty_workspace(containers, idx) {
                visible += 1;
            }
        }

        Some(visible)
    }

    pub fn active_workspace_ref(&self, containers: &ContainerTree<W>) -> Workspace<W> {
        self.ws(containers, self.active_workspace_idx(containers))
    }

    pub fn find_named_workspace(
        &self,
        containers: &ContainerTree<W>,
        workspace_name: &str,
    ) -> Option<Workspace<W>> {
        self.workspaces(containers).find(|ws| {
            ws.name(containers)
                .is_some_and(|name| name.eq_ignore_ascii_case(workspace_name))
        })
    }

    #[cfg(test)]
    pub fn find_named_workspace_index(
        &self,
        containers: &ContainerTree<W>,
        workspace_name: &str,
    ) -> Option<usize> {
        self.workspaces(containers).position(|ws| {
            ws.name(containers)
                .is_some_and(|name| name.eq_ignore_ascii_case(workspace_name))
        })
    }

    pub fn active_workspace(&self, containers: &ContainerTree<W>) -> Workspace<W> {
        self.ws(containers, self.active_workspace_idx(containers))
    }

    pub(super) fn active_mark_target_key(&self, containers: &ContainerTree<W>) -> Option<NodeKey> {
        containers.workspace_command_node(self.active_workspace_ref(containers).workspace_key())
    }

    pub(super) fn add_scratchpad_tile(
        &mut self,
        containers: &mut ContainerTree<W>,
        tile: IncomingView<W>,
        activate: bool,
    ) {
        let workspace_idx = self.active_workspace_idx(containers);

        {
            let workspace = self.ws(containers, workspace_idx);

            // `root_scratchpad_show`: a scratchpad window comes out over whatever the target
            // workspace was showing fullscreen, and that one is taken out of fullscreen first.
            //
            // sway/tree/root.c:166-170
            workspace.clear_fullscreen(containers);

            workspace.add_scratchpad_tile(containers, tile, activate);
        }
        let workspace_idx = self.claim_workspace(containers, workspace_idx);

        if activate {
            self.activate_workspace(containers, workspace_idx);
        }
    }

    pub fn idx_of_ws(&self, containers: &ContainerTree<W>, id: WorkspaceId) -> Option<usize> {
        self.workspaces(containers).position(|ws| ws.id() == id)
    }

    pub(super) fn workspace_idx_for_window(
        &self,
        containers: &ContainerTree<W>,
        window: &W::Id,
    ) -> Option<usize> {
        let tree = containers;
        let workspace = tree
            .window_key(window)
            .and_then(|key| tree.workspace_of(key))?;
        self.workspace_keys(containers)
            .iter()
            .position(|&candidate| candidate == workspace)
    }

    pub fn has_ws(&self, containers: &ContainerTree<W>, id: WorkspaceId) -> bool {
        self.idx_of_ws(containers, id).is_some()
    }

    pub fn windows<'a>(
        &'a self,
        containers: &'a ContainerTree<W>,
    ) -> impl Iterator<Item = &'a W> + 'a {
        self.workspace_keys(containers)
            .iter()
            .flat_map(move |&ws| containers.windows_in_workspace(ws))
    }

    /// Make the workspace at `idx` the active one, bringing the output's sticky floats along.
    pub(super) fn set_active_workspace_idx(
        &mut self,
        containers: &mut ContainerTree<W>,
        idx: usize,
    ) {
        self.active_workspace = self.ws(containers, idx).workspace_key();
        self.sync_sticky_host(containers);
    }

    /// sway's `seat_set_workspace_focus` tail: when the output starts showing another
    /// workspace, the sticky floats of the one it was showing move across, keeping their
    /// stacking order, above whatever the new one already holds.
    ///
    /// A host that is no longer on this output keeps its floats. They are on that workspace,
    /// not on the output, and sway's `move workspace to output` takes them along the same way.
    ///
    /// sway/input/seat.c:1209-1221
    pub(super) fn sync_sticky_host(&mut self, containers: &mut ContainerTree<W>) {
        let active = WorkspaceId::of(self.active_workspace);
        let previous = std::mem::replace(&mut self.sticky_host, active);
        if previous == active {
            return;
        }
        if let Some(from) = self.idx_of_ws(containers, previous) {
            self.move_sticky_floats(containers, from, self.active_workspace_idx(containers));
        }
    }

    fn move_sticky_floats(&mut self, containers: &mut ContainerTree<W>, from: usize, to: usize) {
        let roots = containers.sticky_roots(self.ws(containers, from).workspace_key());
        self.move_floats(containers, &roots, from, to);
    }

    fn move_floats(
        &mut self,
        containers: &mut ContainerTree<W>,
        roots: &[NodeKey],
        from: usize,
        to: usize,
    ) {
        if roots.is_empty() || from == to {
            return;
        }
        let destination = self.ws(containers, to).workspace_key();
        for (stack_index, root) in roots.iter().copied().enumerate() {
            assert!(containers.move_floating_root_to_workspace(root, destination, stack_index));
        }
        self.ws(containers, from).floats_left(containers);
        self.keep_end_workspaces_empty(containers);
    }

    /// `sticky enable` on a float of a workspace this output is not showing: sway moves it to
    /// the one it is, on top, and lets the workspace it left go if that emptied it.
    ///
    /// sway/commands/sticky.c:28-46
    pub(super) fn bring_float_to_active_workspace(
        &mut self,
        containers: &mut ContainerTree<W>,
        from: usize,
        root: NodeKey,
    ) {
        self.move_floats(
            containers,
            &[root],
            from,
            self.active_workspace_idx(containers),
        );
        if self.workspace_switch.is_none() {
            self.clean_up_workspaces(containers);
        }
    }

    /// Move the sticky floats of every workspace that holds nothing else to `destination`.
    ///
    /// sway's `output_evacuate`: such a workspace is empty as far as the user is concerned,
    /// so it is destroyed rather than moved, and its sticky floats go to the workspace the
    /// next output is showing, centered there.
    ///
    /// sway/tree/output.c:188-241
    pub(super) fn evacuate_sticky_floats(
        &mut self,
        containers: &mut ContainerTree<W>,
        destination: Workspace<W>,
    ) {
        let target = destination.workspace_key();
        for ws in self.workspace_list(containers) {
            if ws.has_non_sticky_windows_or_persistent_identity(containers) {
                continue;
            }
            let roots = containers.sticky_roots(ws.workspace_key());
            if roots.is_empty() {
                continue;
            }
            for (stack_index, root) in roots.iter().copied().enumerate() {
                assert!(containers.move_floating_root_to_workspace(root, target, stack_index));
            }
            ws.floats_left(containers);
            for root in roots {
                destination.center_floating_root(containers, root);
            }
        }
    }

    pub fn add_workspace_at(&mut self, containers: &mut ContainerTree<W>, idx: usize) {
        let ws = Workspace::new(containers, self.output.clone(), self.options.clone());
        ws.attach(containers, self.output_key, idx);

        if let Some(switch) = &mut self.workspace_switch {
            if idx as f64 <= switch.target_idx() {
                switch.offset(1);
            }
        }
    }

    /// Something was just put on `workspace_idx`. It becomes this output's own, and the output
    /// keeps its empty workspaces at the ends. Returns where the workspace is afterwards.
    fn claim_workspace(
        &mut self,
        containers: &mut ContainerTree<W>,
        workspace_idx: usize,
    ) -> usize {
        let workspace = self.ws(containers, workspace_idx);
        workspace.claim_for_output(containers, &self.output);
        let id = workspace.id();
        self.keep_end_workspaces_empty(containers);
        self.idx_of_ws(containers, id).unwrap()
    }

    /// Put an empty workspace back at each end that something now occupies.
    ///
    /// The output's sticky floats occupy a workspace like any window here: arriving on the
    /// empty one at the end with them is like opening a window there, and leaving it empties
    /// it again.
    pub(super) fn keep_end_workspaces_empty(&mut self, containers: &mut ContainerTree<W>) {
        if self
            .last_ws(containers)
            .has_windows_or_persistent_identity(containers)
        {
            self.add_workspace_bottom(containers);
        }
        if self.options.layout.empty_workspace_above_first
            && self
                .ws(containers, 0)
                .has_windows_or_persistent_identity(containers)
        {
            self.add_workspace_top(containers);
        }
    }

    pub fn add_workspace_top(&mut self, containers: &mut ContainerTree<W>) {
        self.add_workspace_at(containers, 0);
    }

    pub fn add_workspace_bottom(&mut self, containers: &mut ContainerTree<W>) {
        self.add_workspace_at(containers, self.workspace_count(containers));
    }

    fn normalize_numeric_workspace_order(&mut self, containers: &mut ContainerTree<W>) {
        let start = if self.options.layout.empty_workspace_above_first {
            1
        } else {
            0
        };
        let end = self.workspace_count(containers).saturating_sub(1);
        if start >= end {
            return;
        }

        let identities: std::collections::HashMap<NodeKey, WorkspaceIdentity> = self
            .workspace_keys(containers)
            .iter()
            .map(|&key| (key, containers.workspace_properties(key).identity.clone()))
            .collect();
        containers.reorder_output_workspaces(self.output_key, |workspaces| {
            workspaces[start..end].sort_by(|a, b| match (&identities[a], &identities[b]) {
                (
                    WorkspaceIdentity::Numeric { number: a, .. },
                    WorkspaceIdentity::Numeric { number: b, .. },
                ) => a.cmp(b),
                (WorkspaceIdentity::Numeric { .. }, _) => std::cmp::Ordering::Less,
                (_, WorkspaceIdentity::Numeric { .. }) => std::cmp::Ordering::Greater,
                _ => std::cmp::Ordering::Equal,
            });
        });
    }

    pub(super) fn set_initial_numeric_workspace(
        &mut self,
        containers: &mut ContainerTree<W>,
        number: u32,
        lifetime: WorkspaceLifetime,
    ) {
        if self.options.layout.empty_workspace_above_first
            && self.workspace_count(containers) == 1
            && self.active_workspace_idx(containers) == 0
        {
            self.add_workspace_bottom(containers);
            self.ws(containers, 1)
                .set_numeric_identity(containers, number, lifetime);
            self.add_workspace_bottom(containers);
            self.set_active_workspace_idx(containers, 1);
            return;
        }

        let idx = self.active_workspace_idx(containers);
        self.ws(containers, idx)
            .set_numeric_identity(containers, number, lifetime);

        if idx == self.workspace_count(containers) - 1 {
            self.add_workspace_bottom(containers);
        }
    }

    pub(super) fn add_numeric_workspace(
        &mut self,
        containers: &mut ContainerTree<W>,
        number: u32,
        lifetime: WorkspaceLifetime,
    ) -> usize {
        let idx = self.workspace_count(containers).saturating_sub(1);
        self.add_workspace_at(containers, idx);
        self.ws(containers, idx)
            .set_numeric_identity(containers, number, lifetime);
        self.normalize_numeric_workspace_order(containers);
        self.workspaces(containers)
            .position(|ws| ws.numeric_number(containers) == Some(number))
            .unwrap_or(idx)
    }

    pub(super) fn named_workspace_insert_idx(&self, containers: &ContainerTree<W>) -> usize {
        let start = if self.options.layout.empty_workspace_above_first {
            1
        } else {
            0
        };
        let end = self.workspace_count(containers).saturating_sub(1);
        if start >= end {
            return end;
        }

        self.workspaces(containers)
            .take(end)
            .skip(start)
            .position(|ws| ws.numeric_number(containers).is_none())
            .map(|idx| start + idx)
            .unwrap_or(end)
    }

    pub fn activate_workspace(&mut self, containers: &mut ContainerTree<W>, idx: usize) {
        self.activate_workspace_with_anim_config(containers, idx, None);
    }

    pub fn activate_workspace_with_anim_config(
        &mut self,
        containers: &mut ContainerTree<W>,
        idx: usize,
        config: Option<tiri_config::Animation>,
    ) {
        let prev_active_id = self
            .ws(containers, self.active_workspace_idx(containers))
            .id();
        if self.active_workspace_idx(containers) != idx {
            self.previous_workspace_id = Some(prev_active_id);
        }

        // Read back afterwards: sticky floats arriving on the empty workspace at the start
        // put a new one in front of it, which shifts every index including the switch's.
        self.set_active_workspace_idx(containers, idx);
        let idx = self.active_workspace_idx(containers);
        // FIXME: also compute and use current velocity.
        let current_idx = self.workspace_render_idx(containers);

        let config = config.unwrap_or(self.options.animations.workspace_switch.0);

        match &mut self.workspace_switch {
            // During a DnD scroll, we want to visually animate even if idx matches the active idx.
            Some(WorkspaceSwitch::Gesture(gesture)) if gesture.dnd_last_event_time.is_some() => {
                gesture.center_idx = idx;

                // Adjust start_idx to make current_idx point at idx.
                let current_pos = gesture.current_idx - gesture.start_idx;
                gesture.start_idx = idx as f64 - current_pos;
                let prev_current_idx = gesture.current_idx;
                gesture.current_idx = idx as f64;

                let current_idx_delta = gesture.current_idx - prev_current_idx;
                gesture.animate_from(-current_idx_delta, self.clock.clone(), config);
            }
            _ => {
                // Don't animate if nothing changed.
                if prev_active_id == self.ws(containers, idx).id() {
                    return;
                }

                self.workspace_switch = Some(WorkspaceSwitch::Animation(Animation::new(
                    self.clock.clone(),
                    current_idx,
                    idx as f64,
                    0.,
                    config,
                )));
            }
        }
    }

    pub(super) fn resolve_add_window_target<'a>(
        &mut self,
        containers: &mut ContainerTree<W>,
        target: MonitorAddWindowTarget<'a, W>,
    ) -> (usize, WorkspaceAddWindowTarget<'a, W>) {
        match target {
            MonitorAddWindowTarget::Auto => (
                self.active_workspace_idx(containers),
                WorkspaceAddWindowTarget::Auto,
            ),
            MonitorAddWindowTarget::Workspace { id, root_index } => {
                let idx = self.idx_of_ws(containers, id).unwrap();
                let target = if let Some(root_index) = root_index {
                    WorkspaceAddWindowTarget::AtRootIndex(root_index)
                } else {
                    WorkspaceAddWindowTarget::Auto
                };
                (idx, target)
            }
            MonitorAddWindowTarget::NextTo(win_id) => {
                let idx = self.workspace_idx_for_window(containers, win_id).unwrap();
                (idx, WorkspaceAddWindowTarget::NextTo(win_id))
            }
        }
    }

    pub fn add_window(
        &mut self,
        containers: &mut ContainerTree<W>,
        window: W,
        target: MonitorAddWindowTarget<W>,
        activate: ActivateWindow,
        is_floating: bool,
    ) {
        // Currently, everything a workspace sets on a Tile is the same across all workspaces of a
        // monitor. So we can use any workspace, not necessarily the exact target workspace.
        let tile = self.ws(containers, 0).make_tile(containers, window);

        self.add_tile(containers, tile.into(), target, activate, true, is_floating);
    }

    pub fn add_root_tiling_subtree(
        &mut self,
        containers: &mut ContainerTree<W>,
        workspace_idx: usize,
        subtree: RootTilingSubtree,
        activate: bool,
    ) {
        let workspace = self.ws(containers, workspace_idx);

        workspace.add_root_tiling_subtree(containers, subtree, activate);
        let workspace_idx = self.claim_workspace(containers, workspace_idx);

        if activate {
            self.activate_workspace(containers, workspace_idx);
        }
    }

    #[allow(clippy::too_many_arguments)]
    pub(super) fn add_tile(
        &mut self,
        containers: &mut ContainerTree<W>,
        tile: IncomingView<W>,
        target: MonitorAddWindowTarget<W>,
        activate: ActivateWindow,
        // FIXME: Refactor ActivateWindow enum to make this better.
        allow_to_activate_workspace: bool,
        is_floating: bool,
    ) {
        let (workspace_idx, target) = self.resolve_add_window_target(containers, target);

        let workspace = self.ws(containers, workspace_idx);

        workspace.add_tile(containers, tile, target, activate, is_floating);
        let workspace_idx = self.claim_workspace(containers, workspace_idx);

        if allow_to_activate_workspace && activate.map_smart(|| false) {
            self.activate_workspace(containers, workspace_idx);
        }
    }

    #[allow(clippy::too_many_arguments)]
    pub(super) fn add_tile_split(
        &mut self,
        containers: &mut ContainerTree<W>,
        workspace_idx: usize,
        target: NodeKey,
        direction: Direction,
        tile: IncomingView<W>,
        activate: bool,
        allow_to_activate_workspace: bool,
    ) -> Result<(), IncomingView<W>> {
        {
            let workspace = self.ws(containers, workspace_idx);
            workspace.add_tile_split(containers, target, direction, tile, activate)?;
        }
        let workspace_idx = self.claim_workspace(containers, workspace_idx);

        if allow_to_activate_workspace && activate {
            self.activate_workspace(containers, workspace_idx);
        }

        Ok(())
    }

    pub(super) fn add_tile_split_root(
        &mut self,
        containers: &mut ContainerTree<W>,
        workspace_idx: usize,
        direction: Direction,
        tile: IncomingView<W>,
        activate: bool,
        allow_to_activate_workspace: bool,
    ) -> Result<(), IncomingView<W>> {
        {
            let workspace = self.ws(containers, workspace_idx);
            workspace.add_tile_split_root(containers, direction, tile, activate)?;
        }
        let workspace_idx = self.claim_workspace(containers, workspace_idx);

        if allow_to_activate_workspace && activate {
            self.activate_workspace(containers, workspace_idx);
        }

        Ok(())
    }

    pub fn clean_up_workspaces(&mut self, containers: &mut ContainerTree<W>) {
        assert!(self.workspace_switch.is_none());

        for idx in (0..self.workspace_count(containers)).rev() {
            let is_internal_placeholder = idx == self.workspace_count(containers) - 1
                || (self.options.layout.empty_workspace_above_first && idx == 0);
            if self.ws(containers, idx).should_remove_when_empty(
                containers,
                self.active_workspace_idx(containers) == idx,
                is_internal_placeholder,
            ) {
                self.remove_workspace_at(containers, idx);
            }
        }

        // Special case handling when empty_workspace_above_first is set and all workspaces
        // are empty.
        if self.options.layout.empty_workspace_above_first && self.workspace_count(containers) == 2
        {
            assert!(!self
                .ws(containers, 0)
                .has_windows_or_persistent_identity(containers));
            assert!(!self
                .ws(containers, 1)
                .has_windows_or_persistent_identity(containers));
            self.remove_workspace_at(containers, 1);
            self.active_workspace = self.ws(containers, 0).workspace_key();
        }

        // Every cleanup ends here, and the one above can change which workspace is shown.
        self.sync_sticky_host(containers);
    }

    pub fn unname_workspace(&mut self, containers: &mut ContainerTree<W>, id: WorkspaceId) -> bool {
        let Some(idx) = self.idx_of_ws(containers, id) else {
            return false;
        };
        let ws = self.ws(containers, idx);

        ws.unname(containers);

        if self.workspace_switch.is_none() {
            self.clean_up_workspaces(containers);
        }

        true
    }

    pub fn remove_workspace_by_idx(
        &mut self,
        containers: &mut ContainerTree<W>,
        mut idx: usize,
    ) -> Workspace<W> {
        if idx == self.workspace_count(containers) - 1 {
            self.add_workspace_bottom(containers);
        }
        if self.options.layout.empty_workspace_above_first && idx == 0 {
            self.add_workspace_top(containers);
            idx += 1;
        }

        let ws = self.detach_workspace_at(containers, idx);

        // For monitor current workspace removal, we focus previous rather than next. This is
        // different from columns and tiles, but it lets move-workspace-to-monitor back and
        // forth to preserve position.
        if ws.workspace_key() == self.active_workspace {
            self.active_workspace = self.ws(containers, idx.saturating_sub(1)).workspace_key();
        }
        // The removed workspace may have been the one shown; whichever is shown now holds the
        // output's sticky floats from here on. The removed one keeps its own.
        self.sync_sticky_host(containers);

        self.workspace_switch = None;
        self.clean_up_workspaces(containers);

        ws
    }

    pub fn insert_workspace(
        &mut self,
        containers: &mut ContainerTree<W>,
        ws: Workspace<W>,
        mut idx: usize,
        activate: bool,
    ) {
        ws.make_identity_persistent(containers);
        let inserted_id = ws.id();

        // Don't insert past the last empty workspace.
        if idx == self.workspace_count(containers) {
            idx -= 1;
        }
        if idx == 0 && self.options.layout.empty_workspace_above_first {
            // Insert a new empty workspace on top to prepare for insertion of new workspace.
            self.add_workspace_top(containers);
            idx += 1;
        }

        ws.attach(containers, self.output_key, idx);
        ws.update_config(containers, self.options.clone());

        self.normalize_numeric_workspace_order(containers);

        if activate {
            let idx = self.idx_of_ws(containers, inserted_id).unwrap();
            self.workspace_switch = None;
            self.activate_workspace(containers, idx);
        }

        self.workspace_switch = None;
        self.clean_up_workspaces(containers);
    }

    pub fn append_workspaces(
        &mut self,
        containers: &mut ContainerTree<W>,
        workspaces: Vec<Workspace<W>>,
    ) {
        if workspaces.is_empty() {
            return;
        }

        // Push the workspaces from the removed monitor in the end, right before the
        // last, empty, workspace.
        for ws in workspaces {
            let idx = self.workspace_count(containers) - 1;
            ws.attach(containers, self.output_key, idx);
            ws.update_config(containers, self.options.clone());
        }

        // If empty_workspace_above_first is set and the first workspace is now no longer empty,
        // add a new empty workspace on top.
        if self.options.layout.empty_workspace_above_first
            && self
                .ws(containers, 0)
                .has_windows_or_persistent_identity(containers)
        {
            self.add_workspace_top(containers);
        }

        self.normalize_numeric_workspace_order(containers);

        // FIXME: if we're adding workspaces to currently invisible positions
        // (outside the workspace switch), we don't need to cancel it.
        self.workspace_switch = None;
        self.clean_up_workspaces(containers);
    }

    /// Move up or down within the workspace, or, at its edge, to the workspace that way.
    pub fn move_vertically_or_to_workspace(
        &mut self,
        containers: &mut ContainerTree<W>,
        direction: Direction,
    ) {
        if self
            .active_workspace(containers)
            .move_in_direction(containers, direction)
        {
            return;
        }
        match direction {
            Direction::Up | Direction::Down => {
                let target = if direction == Direction::Up {
                    TransferTarget::WorkspaceUp
                } else {
                    TransferTarget::WorkspaceDown
                };
                let idx = self.workspace_idx_for(containers, target).unwrap();
                self.move_to_workspace(containers, None, idx, ActivateWindow::Smart);
            }
            Direction::Left | Direction::Right => {}
        }
    }

    /// Focus up or down within the workspace, or, at its edge, switch to the workspace that way.
    pub fn focus_vertically_or_workspace(
        &mut self,
        containers: &mut ContainerTree<W>,
        direction: Direction,
    ) {
        if self
            .active_workspace(containers)
            .focus_in_direction(containers, direction, true)
        {
            return;
        }
        match direction {
            Direction::Up => self.switch_workspace(containers, WorkspaceTarget::Up),
            Direction::Down => self.switch_workspace(containers, WorkspaceTarget::Down),
            Direction::Left | Direction::Right => {}
        }
    }
    /// The index a relative or absolute workspace target names on this output.
    pub(super) fn workspace_idx_for(
        &self,
        containers: &ContainerTree<W>,
        target: TransferTarget<'_>,
    ) -> Option<usize> {
        let last = self.workspace_count(containers) - 1;
        match target {
            TransferTarget::WorkspaceUp => {
                Some(self.active_workspace_idx(containers).saturating_sub(1))
            }
            TransferTarget::WorkspaceDown => {
                Some(min(self.active_workspace_idx(containers) + 1, last))
            }
            TransferTarget::WorkspaceIndex(idx) => Some(min(idx, last)),
            TransferTarget::Workspace(_) | TransferTarget::Output { .. } => None,
        }
    }

    /// Move `subject` to the workspace at `idx` on this output.
    pub(super) fn transfer_to_workspace(
        &mut self,
        containers: &mut ContainerTree<W>,
        subject: TransferSubject<'_, W::Id>,
        idx: usize,
        activate: ActivateWindow,
    ) {
        let activate_container = !matches!(activate, ActivateWindow::No);
        match subject {
            TransferSubject::Window(window) => {
                self.move_to_workspace(containers, window, idx, activate)
            }
            TransferSubject::Container => {
                self.move_container_to_workspace_idx(containers, idx, activate_container)
            }
            TransferSubject::RootContainer => {
                self.move_column_to_workspace(containers, idx, activate_container)
            }
        }
    }

    pub fn move_to_workspace(
        &mut self,
        containers: &mut ContainerTree<W>,
        window: Option<&W::Id>,
        idx: usize,
        activate: ActivateWindow,
    ) {
        let source_workspace_idx = if let Some(window) = window {
            self.workspace_idx_for_window(containers, window).unwrap()
        } else {
            self.active_workspace_idx(containers)
        };
        let source_id = self.ws(containers, source_workspace_idx).id();

        let new_idx = min(idx, self.workspace_count(containers) - 1);
        if new_idx == source_workspace_idx {
            return;
        }
        let new_id = self.ws(containers, new_idx).id();

        let activate = activate.map_smart(|| {
            window.is_none_or(|win| self.active_window(containers).map(|win| win.id()) == Some(win))
        });

        let workspace = self.ws(containers, source_workspace_idx);
        let Some(window) =
            window.or_else(|| workspace.active_window(containers).map(|win| win.id()))
        else {
            return;
        };
        let window = window.clone();

        // A sticky float is on whichever workspace this output shows, so another workspace
        // of the same output is not somewhere it can be put. sway refuses the command.
        //
        // sway/commands/move.c:542-545
        if containers
            .window_key(&window)
            .is_some_and(|key| containers.is_sticky_or_child(key))
        {
            return;
        }

        // None when no arrange has described this window yet. `tiles_with_render_positions`
        // reads the layout cache, and a tile is in the tree from the moment it is added —
        // being laid out is a later, separate event. Upstream unwraps the same lookup because
        // its positions come from the live column vector, where the two are one state.
        //
        // The move itself does not depend on this; only the animation, which has nothing to
        // travel from.
        let mut old_render_pos = workspace
            .tiles_with_render_positions(containers)
            .find_map(|(tile, offset, _visible)| (tile.window().id() == &window).then_some(offset));

        let transaction = Transaction::new();
        let transferred = workspace.take_tile_for_transfer(containers, &window, transaction);
        transferred.prepare_for_workspace_move(containers);

        // If the view is following the tile, match the animation.
        let config = if activate {
            self.options.animations.workspace_switch.0
        } else {
            self.options.animations.window_movement.0
        };

        let is_floating = transferred.is_floating;
        self.add_tile(
            containers,
            transferred.into_incoming(),
            MonitorAddWindowTarget::Workspace {
                id: new_id,
                root_index: None,
            },
            if activate {
                ActivateWindow::Yes
            } else {
                ActivateWindow::No
            },
            true,
            is_floating,
        );

        if self.workspace_switch.is_none() {
            self.clean_up_workspaces(containers);
        }

        let new_idx = self.idx_of_ws(containers, new_id).unwrap();

        // Animate vertical movement between workspaces.
        //
        // Recompute the source idx in case some workspace was removed during clean-up. If the
        // source workspace itself was removed, don't bother animating this since the removal is
        // instant anyway.
        if let Some(source_workspace_idx) = self.idx_of_ws(containers, source_id) {
            let workspace_delta =
                self.workspace_size_with_gap(1.).h * (source_workspace_idx as f64 - new_idx as f64);
            if let Some(old_render_pos) = &mut old_render_pos {
                old_render_pos.y += workspace_delta;
            }
        }

        let workspace = self.ws(containers, new_idx).workspace_key();
        let found = containers
            .workspace_tiles_with_render_positions_mut(workspace, false)
            .find(|(tile, _)| tile.window().id() == &window);
        if let (Some(old_render_pos), Some((tile, new_render_pos))) = (old_render_pos, found) {
            tile.animate_move_from_with_config(old_render_pos - new_render_pos, config);
            tile.set_anim_y_between_workspaces();
        }
    }

    fn move_container_to_workspace_idx(
        &mut self,
        containers: &mut ContainerTree<W>,
        new_idx: usize,
        activate: bool,
    ) {
        let source_workspace_idx = self.active_workspace_idx(containers);
        if new_idx == source_workspace_idx {
            return;
        }

        let workspace = self.ws(containers, source_workspace_idx);
        if workspace.floating_is_active(containers) {
            let activate = if activate {
                ActivateWindow::Smart
            } else {
                ActivateWindow::No
            };
            self.move_to_workspace(containers, None, new_idx, activate);
            return;
        }

        let Some(mut subtree) = workspace.remove_active_tiling_subtree(containers) else {
            return;
        };
        subtree.prepare_for_workspace_move(containers);

        self.add_root_tiling_subtree(containers, new_idx, subtree, activate);
    }

    fn move_column_to_workspace(
        &mut self,
        containers: &mut ContainerTree<W>,
        idx: usize,
        activate: bool,
    ) {
        let source_workspace_idx = self.active_workspace_idx(containers);

        let new_idx = min(idx, self.workspace_count(containers) - 1);
        if new_idx == source_workspace_idx {
            return;
        }

        let workspace = self.ws(containers, source_workspace_idx);
        if workspace.floating_is_active(containers) {
            let activate = if activate {
                ActivateWindow::Smart
            } else {
                ActivateWindow::No
            };
            self.move_to_workspace(containers, None, idx, activate);
            return;
        }

        let Some(mut subtree) = workspace.remove_active_root_tiling_subtree(containers) else {
            return;
        };
        subtree.prepare_for_workspace_move(containers);

        self.add_root_tiling_subtree(containers, new_idx, subtree, activate);
    }

    /// Show another workspace on this output.
    pub fn switch_workspace(&mut self, containers: &mut ContainerTree<W>, target: WorkspaceTarget) {
        let last = self.workspace_count(containers) - 1;
        // During a DnD scroll, up and down are relative to the workspace that appears to be
        // shown rather than to the active one.
        let dnd_idx = match &self.workspace_switch {
            Some(WorkspaceSwitch::Gesture(gesture)) if gesture.dnd_last_event_time.is_some() => {
                Some(gesture.current_idx)
            }
            _ => None,
        };
        let idx = match target {
            WorkspaceTarget::Up => match dnd_idx {
                Some(current) => (current.ceil() - 1.).clamp(0., last as f64) as usize,
                None => self.active_workspace_idx(containers).saturating_sub(1),
            },
            WorkspaceTarget::Down => match dnd_idx {
                Some(current) => (current.floor() + 1.).clamp(0., last as f64) as usize,
                None => min(self.active_workspace_idx(containers) + 1, last),
            },
            WorkspaceTarget::Index(idx) => min(idx, last),
            // i3's `workspace_auto_back_and_forth`: asking for the workspace already shown
            // goes back to the previous one.
            WorkspaceTarget::IndexOrBack(idx)
                if min(idx, last) != self.active_workspace_idx(containers) =>
            {
                min(idx, last)
            }
            WorkspaceTarget::IndexOrBack(_) | WorkspaceTarget::Previous => {
                let Some(idx) = self.previous_workspace_idx(containers) else {
                    return;
                };
                idx
            }
        };
        self.activate_workspace(containers, idx);
    }

    fn previous_workspace_idx(&self, containers: &ContainerTree<W>) -> Option<usize> {
        let id = self.previous_workspace_id?;
        self.idx_of_ws(containers, id)
    }

    pub fn active_window<'a>(&'a self, containers: &'a ContainerTree<W>) -> Option<&'a W> {
        self.active_workspace_ref(containers)
            .active_window(containers)
    }

    /// Advance the workspace switch; the workspaces' own animations are advanced by the layout
    /// with every other workspace's.
    pub fn advance_animations(&mut self, containers: &mut ContainerTree<W>) {
        match &mut self.workspace_switch {
            Some(WorkspaceSwitch::Animation(anim)) => {
                if anim.is_done() {
                    self.workspace_switch = None;
                    self.clean_up_workspaces(containers);
                }
            }
            Some(WorkspaceSwitch::Gesture(gesture)) => {
                // Make sure the last event time doesn't go too much out of date (for
                // monitors not under cursor), causing sudden jumps.
                //
                // This happens after any dnd_scroll_gesture_scroll() calls (in
                // Layout::advance_animations()), so it doesn't mess up the time delta there.
                if let Some(last_time) = &mut gesture.dnd_last_event_time {
                    let now = self.clock.now_unadjusted();
                    if *last_time != now {
                        *last_time = now;

                        // If last_time was already == now, then dnd_scroll_gesture_scroll() must've
                        // updated the gesture already. Therefore, when this code runs, the pointer
                        // must be outside the DnD scrolling zone.
                        gesture.dnd_nonzero_start_time = None;
                    }
                }

                if let Some(anim) = &mut gesture.animation {
                    if anim.is_done() {
                        gesture.animation = None;
                    }
                }
            }
            None => (),
        }
    }

    pub(super) fn are_animations_ongoing(&self, containers: &ContainerTree<W>) -> bool {
        self.workspace_switch
            .as_ref()
            .is_some_and(|s| s.is_animation_ongoing())
            || self
                .workspaces(containers)
                .any(|ws| ws.are_animations_ongoing(containers))
    }

    pub fn are_transitions_ongoing(&self, containers: &ContainerTree<W>) -> bool {
        self.workspace_switch.is_some()
            || self
                .workspaces(containers)
                .any(|ws| ws.are_transitions_ongoing(containers))
    }

    pub fn update_render_elements(&mut self, containers: &mut ContainerTree<W>, is_active: bool) {
        let mut insert_hint_ws_geo = None;
        let insert_hint_ws_id = self
            .insert_hint
            .as_ref()
            .and_then(|hint| hint.workspace.existing_id());

        for ws in self.workspace_list(containers) {
            ws.update_render_elements(containers, is_active, RenderLayer::MovingBetweenWorkspaces);
        }

        let visible: Vec<_> = self.workspaces_with_render_geo(containers).collect();
        for (ws, geo) in visible {
            ws.update_render_elements(containers, is_active, RenderLayer::Normal);

            if Some(ws.id()) == insert_hint_ws_id {
                insert_hint_ws_geo = Some(geo);
            }
        }

        self.insert_hint_render_loc = None;
        if let Some(hint) = &self.insert_hint {
            match hint.workspace {
                InsertWorkspace::Existing(ws_id) => {
                    if let Some(idx) = self.idx_of_ws(containers, ws_id) {
                        let ws = self.ws(containers, idx);
                        if let Some(mut area) = ws.insert_hint_area(containers, &hint.position) {
                            let scale = ws.scale(containers).fractional_scale();
                            let view_size = containers.view_size(ws.workspace_key());

                            // Make sure the hint is at least partially visible.
                            if matches!(&hint.position, InsertPosition::RootStart) {
                                let zoom = self.overview_zoom();
                                let geo = insert_hint_ws_geo.unwrap();
                                let geo = geo.downscale(zoom);

                                area.loc.x = area.loc.x.max(-geo.loc.x - area.size.w / 2.);
                                area.loc.x =
                                    area.loc.x.min(geo.loc.x + geo.size.w - area.size.w / 2.);
                            }

                            // Round to physical pixels.
                            area = area.to_physical_precise_round(scale).to_logical(scale);

                            let view_rect = Rectangle::new(area.loc.upscale(-1.), view_size);
                            self.insert_hint_element.update_render_elements(
                                area.size,
                                view_rect,
                                hint.corner_radius,
                                scale,
                            );
                            self.insert_hint_render_loc = Some(InsertHintRenderLoc {
                                workspace: hint.workspace,
                                location: area.loc,
                            });
                        }
                    } else {
                        error!("insert hint workspace missing from monitor");
                    }
                }
                InsertWorkspace::NewAt(ws_idx) => {
                    let scale = self.scale().fractional_scale();
                    let zoom = self.overview_zoom();
                    let gap = self.workspace_gap(zoom);

                    let hint_gap = round_logical_in_physical(scale, gap * 0.1);
                    let hint_height = gap - hint_gap * 2.;

                    let next_ws_geo = self.workspaces_render_geo(containers).nth(ws_idx).unwrap();
                    let hint_width = round_logical_in_physical(scale, next_ws_geo.size.w * 0.75);
                    let hint_x =
                        round_logical_in_physical(scale, (next_ws_geo.size.w - hint_width) / 2.);

                    let hint_loc_diff = Point::from((-hint_x, hint_height + hint_gap));
                    let hint_loc = next_ws_geo.loc - hint_loc_diff;
                    let hint_size = Size::from((hint_width, hint_height));

                    // Sometimes the hint ends up 1 px wider than necessary and/or 1 px
                    // narrower than necessary. The values here seem correct. Might have to do with
                    // how zooming out currently doesn't round to output scale properly.

                    // Compute view rect as if we're above the next workspace (rather than below
                    // the previous one).
                    let view_rect = Rectangle::new(hint_loc_diff, next_ws_geo.size);

                    self.insert_hint_element.update_render_elements(
                        hint_size,
                        view_rect,
                        CornerRadius::default(),
                        scale,
                    );
                    self.insert_hint_render_loc = Some(InsertHintRenderLoc {
                        workspace: hint.workspace,
                        location: hint_loc,
                    });
                }
            }
        }
    }

    pub fn update_config(&mut self, containers: &mut ContainerTree<W>, base_options: Rc<Options>) {
        let options =
            Rc::new(Options::clone(&base_options).with_merged_layout(self.layout_config.as_ref()));

        if self.options.layout.empty_workspace_above_first
            != options.layout.empty_workspace_above_first
            && self.workspace_count(containers) > 1
        {
            if options.layout.empty_workspace_above_first {
                self.add_workspace_top(containers);
            } else if self.workspace_switch.is_none() && self.active_workspace_idx(containers) != 0
            {
                let active_idx = self.active_workspace_idx(containers);
                self.move_sticky_floats(containers, 0, active_idx);
                self.remove_workspace_at(containers, 0);
            }
        }

        if options.layout.struts != self.options.layout.struts {
            self.working_area = compute_working_area(
                &self.output,
                options.layout.struts,
                self.scale().fractional_scale(),
            );
        }

        for ws in self.workspace_list(containers) {
            ws.update_config(containers, options.clone());
        }

        self.insert_hint_element
            .update_config(options.layout.insert_hint);

        self.sync_sticky_host(containers);
        self.base_options = base_options;
        self.options = options;
    }

    pub fn update_layout_config(
        &mut self,
        containers: &mut ContainerTree<W>,
        layout_config: Option<tiri_config::LayoutPart>,
    ) -> bool {
        if self.layout_config == layout_config {
            return false;
        }

        self.layout_config = layout_config;
        self.update_config(containers, self.base_options.clone());

        true
    }

    /// The output's own shaders; its workspaces' are updated by the layout with every other
    /// workspace's.
    pub fn update_shaders(&mut self) {
        self.insert_hint_element.update_shaders();
    }

    pub fn update_output_size(&mut self, containers: &mut ContainerTree<W>) {
        self.working_area = compute_working_area(
            &self.output,
            self.options.layout.struts,
            self.scale().fractional_scale(),
        );

        for ws in self.workspace_list(containers) {
            ws.update_output_size(containers);
        }
    }

    pub fn move_workspace_down(&mut self, containers: &mut ContainerTree<W>) {
        if self
            .ws(containers, self.active_workspace_idx(containers))
            .numeric_number(containers)
            .is_some()
        {
            self.normalize_numeric_workspace_order(containers);
            return;
        }

        let mut new_idx = min(
            self.active_workspace_idx(containers) + 1,
            self.workspace_count(containers) - 1,
        );
        if new_idx == self.active_workspace_idx(containers) {
            return;
        }

        let active_idx = self.active_workspace_idx(containers);
        containers.reorder_output_workspaces(self.output_key, |workspaces| {
            workspaces.swap(active_idx, new_idx)
        });

        if new_idx == self.workspace_count(containers) - 1 {
            // Insert a new empty workspace.
            self.add_workspace_bottom(containers);
        }

        if self.options.layout.empty_workspace_above_first && active_idx == 0 {
            self.add_workspace_top(containers);
            new_idx += 1;
        }

        let previous_workspace_id = self.previous_workspace_id;
        self.activate_workspace(containers, new_idx);
        self.workspace_switch = None;
        self.previous_workspace_id = previous_workspace_id;

        self.clean_up_workspaces(containers);
    }

    pub fn move_workspace_up(&mut self, containers: &mut ContainerTree<W>) {
        if self
            .ws(containers, self.active_workspace_idx(containers))
            .numeric_number(containers)
            .is_some()
        {
            self.normalize_numeric_workspace_order(containers);
            return;
        }

        let mut new_idx = self.active_workspace_idx(containers).saturating_sub(1);
        if new_idx == self.active_workspace_idx(containers) {
            return;
        }

        let active_idx = self.active_workspace_idx(containers);
        containers.reorder_output_workspaces(self.output_key, |workspaces| {
            workspaces.swap(active_idx, new_idx)
        });

        if active_idx == self.workspace_count(containers) - 1 {
            // Insert a new empty workspace.
            self.add_workspace_bottom(containers);
        }

        if self.options.layout.empty_workspace_above_first && new_idx == 0 {
            self.add_workspace_top(containers);
            new_idx += 1;
        }

        let previous_workspace_id = self.previous_workspace_id;
        self.activate_workspace(containers, new_idx);
        self.workspace_switch = None;
        self.previous_workspace_id = previous_workspace_id;

        self.clean_up_workspaces(containers);
    }

    pub fn move_workspace_to_idx(
        &mut self,
        containers: &mut ContainerTree<W>,
        old_idx: usize,
        new_idx: usize,
    ) {
        if self.workspace_count(containers) <= old_idx {
            return;
        }

        let new_idx = new_idx.clamp(0, self.workspace_count(containers) - 1);
        if old_idx == new_idx {
            return;
        }

        let moved_workspace_id = self.ws(containers, old_idx).id();
        let moved_had_numeric_identity = self
            .ws(containers, old_idx)
            .numeric_number(containers)
            .is_some();
        containers.reorder_output_workspaces(self.output_key, |workspaces| {
            if old_idx < new_idx {
                workspaces[old_idx..=new_idx].rotate_left(1);
            } else {
                workspaces[new_idx..=old_idx].rotate_right(1);
            }
        });

        if new_idx > old_idx {
            if new_idx == self.workspace_count(containers) - 1 {
                // Insert a new empty workspace.
                self.add_workspace_bottom(containers);
            }

            if self.options.layout.empty_workspace_above_first && old_idx == 0 {
                self.add_workspace_top(containers);
            }
        } else {
            if old_idx == self.workspace_count(containers) - 1 {
                // Insert a new empty workspace.
                self.add_workspace_bottom(containers);
            }

            if self.options.layout.empty_workspace_above_first && new_idx == 0 {
                self.add_workspace_top(containers);
            }
        }

        self.workspace_switch = None;

        if moved_had_numeric_identity {
            if self
                .workspaces(containers)
                .nth(self.active_workspace_idx(containers))
                .is_some_and(|ws| ws.is_empty_transient_numeric(containers, 1))
            {
                let idx = self
                    .workspaces(containers)
                    .position(|ws| ws.id() == moved_workspace_id)
                    .unwrap();
                self.set_active_workspace_idx(containers, idx);
            }

            for ws in self.workspace_list(containers) {
                if ws
                    .numeric_number(containers)
                    .is_some_and(|number| number != 1)
                {
                    ws.make_identity_persistent(containers);
                }
            }
        }

        self.clean_up_workspaces(containers);
    }

    /// Returns the geometry of the active window relative to and clamped to the output.
    ///
    /// During animations, assumes the final view position.
    pub fn active_window_visual_rectangle(
        &self,
        containers: &ContainerTree<W>,
    ) -> Option<Rectangle<f64, Logical>> {
        if self.overview_open {
            return None;
        }

        self.active_workspace_ref(containers)
            .active_window_visual_rectangle(containers)
    }

    fn workspace_size(&self, zoom: f64) -> Size<f64, Logical> {
        let ws_size = self.view_size().upscale(zoom);
        let scale = self.scale().fractional_scale();
        ws_size.to_physical_precise_ceil(scale).to_logical(scale)
    }

    fn workspace_gap(&self, zoom: f64) -> f64 {
        let scale = self.scale().fractional_scale();
        let gap = self.view_size().h * 0.1 * zoom;
        round_logical_in_physical_max1(scale, gap)
    }

    fn workspace_size_with_gap(&self, zoom: f64) -> Size<f64, Logical> {
        let gap = self.workspace_gap(zoom);
        self.workspace_size(zoom) + Size::from((0., gap))
    }

    pub fn overview_zoom(&self) -> f64 {
        let progress = self.overview_progress.as_ref().map(|p| p.value());
        compute_overview_zoom(&self.options, progress)
    }

    pub(super) fn set_overview_progress(
        &mut self,
        containers: &ContainerTree<W>,
        progress: Option<&super::OverviewProgress>,
    ) {
        let prev_render_idx = self.workspace_render_idx(containers);
        self.overview_progress = progress.map(OverviewProgress::from);
        let new_render_idx = self.workspace_render_idx(containers);

        // If the view jumped (can happen when going from corrected to uncorrected render_idx, for
        // example when toggling the overview in the middle of an overview animation), then restart
        // the workspace switch to avoid jumps.
        if prev_render_idx != new_render_idx {
            if let Some(WorkspaceSwitch::Animation(anim)) = &mut self.workspace_switch {
                // FIXME: maintain velocity.
                *anim = anim.restarted(prev_render_idx, anim.to(), 0.);
            }
        }
    }

    pub(super) fn overview_progress_value(&self) -> Option<f64> {
        self.overview_progress.as_ref().map(|p| p.value())
    }

    pub fn workspace_render_idx(&self, containers: &ContainerTree<W>) -> f64 {
        // If workspace switch and overview progress are matching animations, then compute a
        // correction term to make the movement appear monotonic.
        if let (
            Some(WorkspaceSwitch::Animation(switch_anim)),
            Some(OverviewProgress::Animation(progress_anim)),
        ) = (&self.workspace_switch, &self.overview_progress)
        {
            if switch_anim.start_time() == progress_anim.start_time()
                && (switch_anim.duration().as_secs_f64() - progress_anim.duration().as_secs_f64())
                    .abs()
                    <= 0.001
            {
                #[rustfmt::skip]
                // How this was derived:
                //
                // - Assume we're animating a zoom + switch. Consider switch "from" and "to".
                //   These are render_idx values, so first workspace to second would have switch
                //   from = 0. and to = 1. regardless of the zoom level.
                //
                // - At the start, the point at "from" is at Y = 0. We're moving the point at "to"
                //   to Y = 0. We want this to be a monotonic motion in apparent coordinates (after
                //   zoom).
                //
                // - Height at the start:
                //   from_height = (size.h + gap) * from_zoom.
                //
                // - Current height:
                //   current_height = (size.h + gap) * zoom.
                //
                // - We're moving the "to" point to Y = 0:
                //   to_y = 0.
                //
                // - The initial position of the point we're moving:
                //   from_y = (to - from) * from_height.
                //
                // - We want this point to travel monotonically in apparent coordinates:
                //   current_y = from_y + (to_y - from_y) * progress,
                //   where progress is from 0 to 1, equals to the animation progress (switch and
                //   zoom are the same since they are synchronized).
                //
                // - Derive the Y of the first workspace from this:
                //   first_y = current_y - to * current_height.
                //
                // Now, let's substitute and rearrange the terms.
                //
                // - current_y = from_y + (0 - (to - from) * from_height) * progress
                // - progress = (switch_anim.value() - from) / (to - from)
                // - current_y = from_y - (to - from) * from_height * (switch_anim.value() - from) / (to - from)
                // - current_y = from_y - from_height * (switch_anim.value() - from)
                // - first_y = from_y - from_height * (switch_anim.value() - from) - to * current_height
                // - first_y = (to - from) * from_height - from_height * (switch_anim.value() - from) - to * current_height
                // - first_y = to * from_height - switch_anim.value() * from_height - to * current_height
                // - first_y = -switch_anim.value() * from_height + to * (from_height - current_height)
                let from = progress_anim.from();
                let from_zoom = compute_overview_zoom(&self.options, Some(from));
                let from_ws_height_with_gap = self.workspace_size_with_gap(from_zoom).h;

                let zoom = self.overview_zoom();
                let ws_height_with_gap = self.workspace_size_with_gap(zoom).h;

                let first_ws_y = -switch_anim.value() * from_ws_height_with_gap
                    + switch_anim.to() * (from_ws_height_with_gap - ws_height_with_gap);

                return -first_ws_y / ws_height_with_gap;
            }
        };

        if let Some(switch) = &self.workspace_switch {
            switch.current_idx()
        } else {
            self.active_workspace_idx(containers) as f64
        }
    }

    pub fn workspaces_render_geo(
        &self,
        containers: &ContainerTree<W>,
    ) -> impl Iterator<Item = Rectangle<f64, Logical>> {
        let scale = self.scale().fractional_scale();
        let zoom = self.overview_zoom();

        let ws_size = self.workspace_size(zoom);
        let gap = self.workspace_gap(zoom);
        let ws_height_with_gap = ws_size.h + gap;

        let static_offset = (self.view_size().to_point() - ws_size.to_point()).downscale(2.);
        let static_offset = static_offset
            .to_physical_precise_round(scale)
            .to_logical(scale);

        let first_ws_y = -self.workspace_render_idx(containers) * ws_height_with_gap;
        let first_ws_y = round_logical_in_physical(scale, first_ws_y);

        // Return position for one-past-last workspace too.
        (0..=self.workspace_count(containers)).map(move |idx| {
            let y = first_ws_y + idx as f64 * ws_height_with_gap;
            let loc = Point::from((0., y)) + static_offset;

            // Even though all components that go into loc are rounded to physical pixels, the
            // floating point addition may lose precision. This can result for example in the
            // current workspace having y = 0.0000000000002 and thus missing pointer hits at the
            // monitor edge with y = 0. So, post-round the location too.
            let loc = loc.to_physical_precise_round(scale).to_logical(scale);

            Rectangle::new(loc, ws_size)
        })
    }

    pub fn workspaces_with_render_geo_cull<'a>(
        &'a self,
        containers: &'a ContainerTree<W>,
        cull: bool,
    ) -> impl Iterator<Item = (Workspace<W>, Rectangle<f64, Logical>)> + 'a {
        let output_geo = Rectangle::from_size(self.view_size());

        let geo = self.workspaces_render_geo(containers);
        zip(self.workspaces(containers), geo)
            // Cull out workspaces outside the output.
            .filter(move |(_ws, geo)| !cull || geo.intersection(output_geo).is_some())
    }

    pub fn workspaces_with_render_geo<'a>(
        &'a self,
        containers: &'a ContainerTree<W>,
    ) -> impl Iterator<Item = (Workspace<W>, Rectangle<f64, Logical>)> + 'a {
        self.workspaces_with_render_geo_cull(containers, true)
    }

    pub fn workspaces_with_render_geo_idx<'a>(
        &'a self,
        containers: &'a ContainerTree<W>,
    ) -> impl Iterator<Item = ((usize, Workspace<W>), Rectangle<f64, Logical>)> + 'a {
        let output_geo = Rectangle::from_size(self.view_size());

        let geo = self.workspaces_render_geo(containers);
        zip(self.workspaces(containers).enumerate(), geo)
            // Cull out workspaces outside the output.
            .filter(move |(_ws, geo)| geo.intersection(output_geo).is_some())
    }

    pub fn workspace_under(
        &self,
        containers: &ContainerTree<W>,
        pos_within_output: Point<f64, Logical>,
    ) -> Option<(Workspace<W>, Rectangle<f64, Logical>)> {
        let (ws, geo) = self
            .workspaces_with_render_geo(containers)
            .find_map(|(ws, geo)| {
                // Extend width to entire output.
                let loc = Point::from((0., geo.loc.y));
                let size = Size::from((self.view_size().w, geo.size.h));
                let bounds = Rectangle::new(loc, size);

                bounds.contains(pos_within_output).then_some((ws, geo))
            })?;
        Some((ws, geo))
    }

    pub fn workspace_under_narrow(
        &self,
        containers: &ContainerTree<W>,
        pos_within_output: Point<f64, Logical>,
    ) -> Option<Workspace<W>> {
        self.workspaces_with_render_geo(containers)
            .find_map(|(ws, geo)| geo.contains(pos_within_output).then_some(ws))
    }

    pub fn window_under<'a>(
        &'a self,
        containers: &'a ContainerTree<W>,
        pos_within_output: Point<f64, Logical>,
    ) -> Option<(&'a W, HitType)> {
        let (ws, geo) = self.workspace_under(containers, pos_within_output)?;

        if self.overview_progress.is_some() {
            let zoom = self.overview_zoom();
            let pos_within_workspace = (pos_within_output - geo.loc).downscale(zoom);
            let (win, hit) = ws.window_under(containers, pos_within_workspace)?;
            // During the overview animation, we cannot do input hits because we cannot really
            // represent scaled windows properly.
            Some((win, hit.to_activate()))
        } else {
            let (win, hit) = ws.window_under(containers, pos_within_output - geo.loc)?;
            Some((win, hit.offset_win_pos(geo.loc)))
        }
    }

    pub fn resize_hit_under(
        &self,
        containers: &ContainerTree<W>,
        pos_within_output: Point<f64, Logical>,
    ) -> Option<super::ResizeHit<W::Id>> {
        if self.overview_progress.is_some() {
            return None;
        }

        let view_width = self.view_size().w;
        for (ws, geo) in self.workspaces_with_render_geo(containers) {
            let loc = Point::from((0., geo.loc.y));
            let size = Size::from((view_width, geo.size.h));
            let bounds = Rectangle::new(loc, size);
            if bounds.contains(pos_within_output) {
                return ws.resize_hit_under(containers, pos_within_output - geo.loc);
            }
        }

        None
    }

    pub(super) fn insert_position(
        &self,
        containers: &ContainerTree<W>,
        pos_within_output: Point<f64, Logical>,
    ) -> (InsertWorkspace, Rectangle<f64, Logical>) {
        let mut iter = self.workspaces_with_render_geo_idx(containers);

        let dummy = Rectangle::default();

        // Monitors always have at least one workspace.
        let ((idx, ws), geo) = iter.next().unwrap();

        // Check if above first.
        if pos_within_output.y < geo.loc.y {
            return (InsertWorkspace::NewAt(idx), dummy);
        }

        let contains = move |geo: Rectangle<f64, Logical>| {
            geo.loc.y <= pos_within_output.y && pos_within_output.y < geo.loc.y + geo.size.h
        };

        // Check first.
        if contains(geo) {
            return (InsertWorkspace::Existing(ws.id()), geo);
        }

        let mut last_geo = geo;
        let mut last_idx = idx;
        for ((idx, ws), geo) in iter {
            // Check gap above.
            let gap_loc = Point::from((last_geo.loc.x, last_geo.loc.y + last_geo.size.h));
            let gap_size = Size::from((geo.size.w, geo.loc.y - gap_loc.y));
            let gap_geo = Rectangle::new(gap_loc, gap_size);
            if contains(gap_geo) {
                return (InsertWorkspace::NewAt(idx), dummy);
            }

            // Check workspace itself.
            if contains(geo) {
                return (InsertWorkspace::Existing(ws.id()), geo);
            }

            last_geo = geo;
            last_idx = idx;
        }

        // Anything below.
        (InsertWorkspace::NewAt(last_idx + 1), dummy)
    }

    pub fn render_above_top_layer(&self, containers: &ContainerTree<W>) -> bool {
        // Render above the top layer only if the view is stationary.
        if self.workspace_switch.is_some() || self.overview_progress.is_some() {
            return false;
        }

        self.active_workspace(containers)
            .render_above_top_layer(containers)
    }

    /// The window shown fullscreen above the top layer, while the view is stationary.
    pub fn fullscreen_window_above_top_layer<'a>(
        &self,
        containers: &'a ContainerTree<W>,
    ) -> Option<&'a W> {
        if self.workspace_switch.is_some() || self.overview_progress.is_some() {
            return None;
        }

        self.active_workspace(containers)
            .fullscreen_window_above_top_layer(containers)
    }

    pub fn render_insert_hint_between_workspaces<R: NiriRenderer>(
        &self,
        renderer: &mut R,
        push: &mut dyn FnMut(MonitorRenderElement<R>),
    ) {
        if self.options.layout.insert_hint.off {
            return;
        }
        let Some(render_loc) = self.insert_hint_render_loc else {
            return;
        };
        let InsertWorkspace::NewAt(_) = render_loc.workspace else {
            return;
        };

        self.insert_hint_element
            .render(renderer, render_loc.location, &mut |elem| {
                let elem = MonitorInnerRenderElement::UncroppedInsertHint(elem);
                let elem = RescaleRenderElement::from_element(elem, Point::default(), 1.);
                let elem =
                    RelocateRenderElement::from_element(elem, Point::default(), Relocate::Relative);
                push(elem);
            });
    }

    pub fn render_workspaces<R: NiriRenderer>(
        &self,
        containers: &ContainerTree<W>,
        mut ctx: RenderCtx<R>,
        focus_ring: bool,
        push: &mut dyn FnMut(MonitorRenderElement<R>),
    ) {
        let _span = tracy_client::span!("Monitor::render_workspaces");

        let scale = self.scale().fractional_scale();
        // Ceil the height in physical pixels.
        let height = (self.view_size().h * scale).ceil() as i32;

        let zoom = self.overview_zoom();

        let insert_hint_render_loc = self
            .insert_hint_render_loc
            .filter(|_| !self.options.layout.insert_hint.off);

        let scale_relocate = move |geo: Rectangle<f64, Logical>, elem| {
            let elem = RescaleRenderElement::from_element(elem, Point::from((0, 0)), zoom);
            RelocateRenderElement::from_element(
                elem,
                // The offset we get from workspaces_with_render_geo() is already
                // rounded to physical pixels, but it's in the logical coordinate
                // space, so we need to convert it to physical.
                geo.loc.to_physical_precise_round(scale),
                Relocate::Relative,
            )
        };

        let active_ws_id = self
            .ws(containers, self.active_workspace_idx(containers))
            .id();

        // Where the active workspace rests. Its sticky floats are drawn here while it slides:
        // they belong to the output, so a workspace switch is not something they take part in.
        let ws_size = self.workspace_size(zoom);
        let static_offset = (self.view_size().to_point() - ws_size.to_point()).downscale(2.);
        let static_offset = static_offset
            .to_physical_precise_round(scale)
            .to_logical(scale);
        let sticky_geo = Rectangle::new(static_offset, ws_size);

        // Draw in passes for correct Z ordering during window movement between workspaces:
        // - floating windows moving between workspaces
        // - normal floating windows
        // - tiled windows moving between workspaces
        // - normal tiled windows
        for pass in 0..4 {
            // Don't cull when drawing windows moving between workspaces so that windows moving to
            // workspaces off-screen will still render.
            let cull = matches!(pass, 1 | 3);
            let layer = if cull {
                RenderLayer::Normal
            } else {
                RenderLayer::MovingBetweenWorkspaces
            };

            // Crop the elements to prevent them overflowing, currently visible during a workspace
            // switch.
            //
            // HACK: crop to infinite bounds at least horizontally where we
            // know there's no workspace joining or monitor bounds, otherwise
            // it will cut pixel shaders and mess up the coordinate space.
            // There's also a damage tracking bug which causes glitched
            // rendering for maximized GTK windows.
            //
            // FIXME: use proper bounds after fixing the Crop element.
            //
            // Also, check cull here to avoid cropping windows moving between workspaces.
            let crop_bounds =
                if cull && (self.workspace_switch.is_some() || self.overview_progress.is_some()) {
                    Rectangle::new(
                        Point::from((-i32::MAX / 2, 0)),
                        Size::from((i32::MAX, height)),
                    )
                } else {
                    Rectangle::new(
                        Point::from((-i32::MAX / 2, -i32::MAX / 2)),
                        Size::from((i32::MAX, i32::MAX)),
                    )
                };

            for (ws, geo) in self.workspaces_with_render_geo_cull(containers, cull) {
                // Macro instead of closure because ws and insert hint have different elem types.
                macro_rules! push {
                    () => {{
                        &mut |elem| {
                            let elem = CropRenderElement::from_element(elem, scale, crop_bounds);
                            if let Some(elem) = elem {
                                let elem = MonitorInnerRenderElement::from(elem);
                                push(scale_relocate(geo, elem));
                            }
                        }
                    }};
                }

                let xray_pos = XrayPos::new(geo.loc, zoom);

                if pass < 2 {
                    let pin_sticky =
                        layer.is_normal() && ws.id() == active_ws_id && geo.loc != sticky_geo.loc;
                    if pin_sticky {
                        ws.render_floating(
                            containers,
                            ctx.r(),
                            XrayPos::new(sticky_geo.loc, zoom),
                            focus_ring,
                            layer,
                            FloatingRoots::Sticky,
                            &mut |elem| {
                                let elem =
                                    CropRenderElement::from_element(elem, scale, crop_bounds);
                                if let Some(elem) = elem {
                                    let elem = MonitorInnerRenderElement::from(elem);
                                    push(scale_relocate(sticky_geo, elem));
                                }
                            },
                        );
                    }
                    let roots = if pin_sticky {
                        FloatingRoots::NonSticky
                    } else {
                        FloatingRoots::All
                    };
                    ws.render_floating(
                        containers,
                        ctx.r(),
                        xray_pos,
                        focus_ring,
                        layer,
                        roots,
                        push!(),
                    );

                    if layer.is_normal() {
                        if let Some(loc) = insert_hint_render_loc {
                            if loc.workspace == InsertWorkspace::Existing(ws.id()) {
                                self.insert_hint_element.render(
                                    ctx.renderer,
                                    loc.location,
                                    push!(),
                                );
                            }
                        }
                    }
                } else if self.overview_progress.is_some() {
                    // The offscreen pass draws the whole tiling at once, so it has no separate
                    // layer to draw.
                    if layer.is_normal() {
                        ws.render_tiling_as_offscreen(
                            containers,
                            ctx.renderer.as_gles_renderer(),
                            ctx.target,
                            focus_ring,
                            push!(),
                        );
                    }
                } else {
                    ws.render_tiling(containers, ctx.r(), xray_pos, focus_ring, layer, push!());
                }
            }
        }
    }

    pub fn render_workspace_shadows<R: NiriRenderer>(
        &self,
        containers: &ContainerTree<W>,
        renderer: &mut R,
        push: &mut dyn FnMut(MonitorRenderElement<R>),
    ) {
        let Some(progress) = self.overview_progress.as_ref().map(|p| p.clamped_value()) else {
            return;
        };
        let alpha = progress.clamp(0., 1.) as f32;

        let _span = tracy_client::span!("Monitor::render_workspace_shadows");

        let scale = self.scale().fractional_scale();
        let zoom = self.overview_zoom();

        for (ws, geo) in self.workspaces_with_render_geo(containers) {
            ws.render_shadow(containers, renderer, &mut |elem| {
                let elem = elem.with_alpha(alpha);
                let elem = MonitorInnerRenderElement::Shadow(elem);
                let elem = RescaleRenderElement::from_element(elem, Point::from((0, 0)), zoom);
                let elem = RelocateRenderElement::from_element(
                    elem,
                    geo.loc.to_physical_precise_round(scale),
                    Relocate::Relative,
                );
                push(elem);
            });
        }
    }

    pub fn workspace_switch_gesture_begin(
        &mut self,
        containers: &ContainerTree<W>,
        is_touchpad: bool,
    ) {
        let center_idx = self.active_workspace_idx(containers);
        let current_idx = self.workspace_render_idx(containers);

        let gesture = WorkspaceSwitchGesture {
            center_idx,
            start_idx: current_idx,
            current_idx,
            animation: None,
            tracker: SwipeTracker::new(),
            is_touchpad,
            is_clamped: !self.overview_open,
            dnd_last_event_time: None,
            dnd_nonzero_start_time: None,
        };
        self.workspace_switch = Some(WorkspaceSwitch::Gesture(gesture));
    }

    pub fn dnd_scroll_gesture_begin(&mut self, containers: &ContainerTree<W>) {
        if let Some(WorkspaceSwitch::Gesture(WorkspaceSwitchGesture {
            dnd_last_event_time: Some(_),
            ..
        })) = &self.workspace_switch
        {
            // Already active.
            return;
        }

        if !self.overview_open {
            // This gesture is only for the overview.
            return;
        }

        let center_idx = self.active_workspace_idx(containers);
        let current_idx = self.workspace_render_idx(containers);

        let gesture = WorkspaceSwitchGesture {
            center_idx,
            start_idx: current_idx,
            current_idx,
            animation: None,
            tracker: SwipeTracker::new(),
            is_touchpad: false,
            is_clamped: false,
            dnd_last_event_time: Some(self.clock.now_unadjusted()),
            dnd_nonzero_start_time: None,
        };
        self.workspace_switch = Some(WorkspaceSwitch::Gesture(gesture));
    }

    pub fn workspace_switch_gesture_update(
        &mut self,
        containers: &ContainerTree<W>,
        delta_y: f64,
        timestamp: Duration,
        is_touchpad: bool,
    ) -> Option<bool> {
        let workspace_count = self.workspace_count(containers);
        let Some(WorkspaceSwitch::Gesture(gesture)) = &self.workspace_switch else {
            return None;
        };

        if gesture.is_touchpad != is_touchpad || gesture.dnd_last_event_time.is_some() {
            return None;
        }

        let zoom = self.overview_zoom();
        let total_height = if gesture.is_touchpad {
            WORKSPACE_GESTURE_MOVEMENT
        } else {
            self.workspace_size_with_gap(1.).h
        };

        let Some(WorkspaceSwitch::Gesture(gesture)) = &mut self.workspace_switch else {
            return None;
        };

        // Reduce the effect of zoom on the touchpad somewhat.
        let delta_scale = if gesture.is_touchpad {
            (zoom - 1.) / 2.5 + 1.
        } else {
            zoom
        };

        let delta_y = delta_y / delta_scale;
        let mut rubber_band = WORKSPACE_GESTURE_RUBBER_BAND;
        rubber_band.limit /= zoom;

        gesture.tracker.push(delta_y, timestamp);

        let pos = gesture.tracker.pos() / total_height;

        let (min, max) = gesture.min_max(workspace_count);
        let new_idx = gesture.start_idx + pos;
        let new_idx = rubber_band.clamp(min, max, new_idx);

        if gesture.current_idx == new_idx {
            return Some(false);
        }

        gesture.current_idx = new_idx;
        Some(true)
    }

    pub fn dnd_scroll_gesture_scroll(
        &mut self,
        containers: &ContainerTree<W>,
        pos: Point<f64, Logical>,
        speed: f64,
    ) -> bool {
        let workspace_count = self.workspace_count(containers);
        let zoom = self.overview_zoom();
        let view_width = self.view_size().w;

        let Some(WorkspaceSwitch::Gesture(gesture)) = &mut self.workspace_switch else {
            return false;
        };

        let Some(last_time) = gesture.dnd_last_event_time else {
            // Not a DnD scroll.
            return false;
        };

        let config = &self.options.gestures.dnd_edge_workspace_switch;
        let trigger_height = config.trigger_height;

        // Restrict the scrolling horizontally to the strip of workspaces to avoid unwanted trigger
        // after using the hot corner or during horizontal scroll.
        let width = view_width * zoom;
        let x = pos.x - (view_width - width) / 2.;

        // Consider the working area so layer-shell docks and such don't prevent scrolling.
        let y = pos.y - self.working_area.loc.y;
        let height = self.working_area.size.h;

        let y = y.clamp(0., height);
        let trigger_height = trigger_height.clamp(0., height / 2.);

        let delta = if x < 0. || width <= x {
            // Outside the bounds horizontally.
            0.
        } else if y < trigger_height {
            -(trigger_height - y)
        } else if height - y < trigger_height {
            trigger_height - (height - y)
        } else {
            0.
        };

        let delta = if trigger_height < 0.01 {
            // Sanity check for trigger-height 0 or small window sizes.
            0.
        } else {
            // Normalize to [0, 1].
            delta / trigger_height
        };
        let delta = delta * speed;

        let now = self.clock.now_unadjusted();
        gesture.dnd_last_event_time = Some(now);

        if delta == 0. {
            // We're outside the scrolling zone.
            gesture.dnd_nonzero_start_time = None;
            return false;
        }

        let nonzero_start = *gesture.dnd_nonzero_start_time.get_or_insert(now);

        // Delay starting the gesture a bit to avoid unwanted movement when dragging across
        // monitors.
        let delay = Duration::from_millis(u64::from(config.delay_ms));
        if now.saturating_sub(nonzero_start) < delay {
            return true;
        }

        let time_delta = now.saturating_sub(last_time).as_secs_f64();

        let delta = delta * time_delta * config.max_speed;

        gesture.tracker.push(delta, now);

        let total_height = WORKSPACE_DND_EDGE_SCROLL_MOVEMENT;
        let pos = gesture.tracker.pos() / total_height;
        let unclamped = gesture.start_idx + pos;

        let (min, max) = gesture.min_max(workspace_count);
        let clamped = unclamped.clamp(min, max);

        // Make sure that DnD scrolling too much outside the min/max does not "build up".
        gesture.start_idx += clamped - unclamped;
        gesture.current_idx = clamped;

        true
    }

    pub fn workspace_switch_gesture_end(
        &mut self,
        containers: &mut ContainerTree<W>,
        is_touchpad: Option<bool>,
    ) -> bool {
        let workspace_count = self.workspace_count(containers);
        let Some(WorkspaceSwitch::Gesture(gesture)) = &self.workspace_switch else {
            return false;
        };

        if is_touchpad.is_some_and(|x| gesture.is_touchpad != x) {
            return false;
        }

        let zoom = self.overview_zoom();
        let total_height = if gesture.dnd_last_event_time.is_some() {
            WORKSPACE_DND_EDGE_SCROLL_MOVEMENT
        } else if gesture.is_touchpad {
            WORKSPACE_GESTURE_MOVEMENT
        } else {
            self.workspace_size_with_gap(1.).h
        };

        let Some(WorkspaceSwitch::Gesture(gesture)) = &mut self.workspace_switch else {
            return false;
        };

        // Take into account any idle time between the last event and now.
        let now = self.clock.now_unadjusted();
        gesture.tracker.push(0., now);

        let mut rubber_band = WORKSPACE_GESTURE_RUBBER_BAND;
        rubber_band.limit /= zoom;

        let mut velocity = gesture.tracker.velocity() / total_height;
        let current_pos = gesture.tracker.pos() / total_height;
        let pos = gesture.tracker.projected_end_pos() / total_height;

        let (min, max) = gesture.min_max(workspace_count);
        let new_idx = gesture.start_idx + pos;

        let new_idx = new_idx.clamp(min, max);
        let new_idx = new_idx.round() as usize;

        velocity *= rubber_band.clamp_derivative(min, max, gesture.start_idx + current_pos);

        if self.active_workspace_idx(containers) != new_idx {
            self.previous_workspace_id = Some(
                self.ws(containers, self.active_workspace_idx(containers))
                    .id(),
            );
        }

        // Read back afterwards, as in `activate_workspace_with_anim_config`: the gesture is
        // still the current switch, so a workspace put in front of it offsets it too.
        self.set_active_workspace_idx(containers, new_idx);
        let new_idx = self.active_workspace_idx(containers);
        let Some(WorkspaceSwitch::Gesture(gesture)) = &self.workspace_switch else {
            unreachable!("the gesture is replaced only below");
        };
        let from_idx = gesture.current_idx;
        self.workspace_switch = Some(WorkspaceSwitch::Animation(Animation::new(
            self.clock.clone(),
            from_idx,
            new_idx as f64,
            velocity,
            self.options.animations.workspace_switch.0,
        )));

        true
    }

    pub fn dnd_scroll_gesture_end(&mut self, containers: &mut ContainerTree<W>) {
        if !matches!(
            self.workspace_switch,
            Some(WorkspaceSwitch::Gesture(WorkspaceSwitchGesture {
                dnd_last_event_time: Some(_),
                ..
            }))
        ) {
            // Not a DnD scroll.
            return;
        };

        self.workspace_switch_gesture_end(containers, None);
    }

    pub fn scale(&self) -> smithay::output::Scale {
        self.output.current_scale()
    }

    pub fn view_size(&self) -> Size<f64, Logical> {
        output_size(&self.output)
    }

    pub fn working_area(&self) -> Rectangle<f64, Logical> {
        self.working_area
    }

    pub fn layout_config(&self) -> Option<&tiri_config::LayoutPart> {
        self.layout_config.as_ref()
    }

    pub(super) fn verify_invariants(&self, containers: &ContainerTree<W>) {
        use approx::assert_abs_diff_eq;

        let options =
            Options::clone(&self.base_options).with_merged_layout(self.layout_config.as_ref());
        assert_eq!(&*self.options, &options);

        assert_eq!(
            self.sticky_host,
            self.ws(containers, self.active_workspace_idx(containers))
                .id(),
            "the active workspace must be the one holding the output's sticky floats"
        );

        assert!(
            self.workspace_count(containers) > 0,
            "monitor must have at least one workspace"
        );
        assert!(self.active_workspace_idx(containers) < self.workspace_count(containers));

        if let Some(WorkspaceSwitch::Animation(anim)) = &self.workspace_switch {
            let before_idx = anim.from() as usize;
            let after_idx = anim.to() as usize;

            assert!(before_idx < self.workspace_count(containers));
            assert!(after_idx < self.workspace_count(containers));
        }

        assert!(
            !containers.workspace_has_windows(self.last_ws(containers).workspace_key()),
            "monitor must have an empty workspace in the end"
        );
        if self.options.layout.empty_workspace_above_first {
            assert!(
                !containers.workspace_has_windows(self.ws(containers, 0).workspace_key()),
                "first workspace must be empty when empty_workspace_above_first is set"
            )
        }

        assert!(
            self.last_ws(containers).name(containers).is_none(),
            "monitor must have an unnamed workspace in the end"
        );
        if self.options.layout.empty_workspace_above_first {
            assert!(
                self.ws(containers, 0).name(containers).is_none(),
                "first workspace must be unnamed when empty_workspace_above_first is set"
            )
        }

        if self.options.layout.empty_workspace_above_first {
            assert!(
                self.workspace_count(containers) != 2,
                "if empty_workspace_above_first is set there must be just 1 or 3+ workspaces"
            )
        }

        // If there's no workspace switch in progress, there can't be any non-last non-active
        // empty workspaces. If empty_workspace_above_first is set then the first workspace
        // will be empty too.
        let pre_skip = if self.options.layout.empty_workspace_above_first {
            1
        } else {
            0
        };
        if self.workspace_switch.is_none() {
            for (idx, ws) in self
                .workspaces(containers)
                .enumerate()
                .skip(pre_skip)
                .rev()
                // skip last
                .skip(1)
            {
                if idx != self.active_workspace_idx(containers) {
                    assert!(
                        ws.has_windows_or_persistent_identity(containers),
                        "non-active workspace can't be empty and unnamed except the last one"
                    );
                }
            }
        }

        for workspace in self.workspaces(containers) {
            assert_eq!(
                self.scale().integer_scale(),
                workspace.scale(containers).integer_scale()
            );
            assert_eq!(
                self.scale().fractional_scale(),
                workspace.scale(containers).fractional_scale()
            );
            assert_eq!(
                self.view_size(),
                containers.view_size(workspace.workspace_key())
            );

            // Not the monitor's own working area: a named workspace can carry its own struts,
            // and then it reserves a different part of the same output. What must hold is that
            // it reserved it from that output, with the struts it actually has.
            assert_eq!(
                containers.working_area(workspace.workspace_key()),
                compute_working_area(
                    &self.output,
                    containers.options(workspace.workspace_key()).layout.struts,
                    self.scale().fractional_scale()
                ),
                "a workspace's working area must be the output's, inset by its own struts"
            );

            assert_eq!(
                workspace.properties(containers).base_options,
                self.options,
                "workspace options must be synchronized with monitor"
            );
        }

        let scale = self.scale().fractional_scale();
        let iter = self.workspaces_with_render_geo(containers);
        for (_ws, ws_geo) in iter {
            let pos = ws_geo.loc;
            let rounded_pos = pos.to_physical_precise_round(scale).to_logical(scale);

            // Workspace positions must be rounded to physical pixels.
            assert_abs_diff_eq!(pos.x, rounded_pos.x, epsilon = 1e-5);
            assert_abs_diff_eq!(pos.y, rounded_pos.y, epsilon = 1e-5);
        }
    }
}
