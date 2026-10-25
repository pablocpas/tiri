//! Ephemeral interaction and rendering state for one workspace.
//!
//! Kept on the workspace's node in the layout's tree, as a tile keeps its animations on its
//! leaf: the node is the workspace, so whatever the workspace is doing right now goes where the
//! workspace is, and it cannot outlive the node or be looked up in the wrong place. Nothing in
//! here is semantic layout state; the tree's operations never read it to decide topology.

use smithay::utils::{Logical, Size};

use super::{compute_workspace_shadow_config, InteractiveResize};
use crate::layout::closing_window::ClosingWindow;
use crate::layout::container::InteractiveResizeState;
use crate::layout::container_tree::LeafFrameInfo;
use crate::layout::shadow::Shadow;
use crate::layout::{LayoutElement, Options};
use crate::render_helpers::offscreen::OffscreenBuffer;
use crate::render_helpers::solid_color::SolidColorBuffer;

#[derive(Debug)]
pub(in crate::layout) struct WorkspaceRuntime<W: LayoutElement> {
    pub(in crate::layout) tiled: TiledRuntime<W>,
    pub(in crate::layout) floating: FloatingRuntimeState<W>,
    /// The seat generation at which focus moved to the tiled side without lowering the
    /// floating one.
    ///
    /// Which side has focus is the seat's to say (see
    /// [`ContainerTree::focused_layer`](crate::layout::container_tree::ContainerTree::focused_layer)).
    /// This is the one thing it cannot: focus-follows-mouse onto a tiled window leaves the
    /// floats drawn above a tiled fullscreen until focus next changes, and the generation is
    /// how "next" is noticed without every focus change having to clear it.
    pub(in crate::layout) floating_raised_at: Option<u64>,
    /// Whether this workspace owns compositor focus in the current render state.
    pub(in crate::layout) is_active: bool,
    /// This workspace's shadow in the overview.
    pub(in crate::layout) shadow: Shadow,
    /// This workspace's background.
    pub(in crate::layout) background: SolidColorBuffer,
    /// The overview's thumbnail of the tiled side.
    ///
    /// One per workspace: the overview draws every workspace in one frame, each into its own
    /// texture, so a single offscreen would have every thumbnail overwrite the last.
    pub(in crate::layout) overview_offscreen: OffscreenBuffer,
}

#[derive(Debug)]
pub(in crate::layout) struct TiledRuntime<W: LayoutElement> {
    /// Ongoing interactive resize. A window can only be resized in its current branch.
    pub(in crate::layout) interactive_resize: Option<InteractiveResizeState<W>>,
    /// Windows in their closing animation, drawn in the tiled render pass.
    pub(in crate::layout) closing_windows: Vec<ClosingWindow>,
}

#[derive(Debug)]
pub(in crate::layout) struct FloatingRuntimeState<W: LayoutElement> {
    /// Ongoing interactive resize.
    pub(in crate::layout) interactive_resize: Option<InteractiveResize<W>>,
    /// Windows in the closing animation.
    pub(in crate::layout) closing_windows: Vec<ClosingWindow>,
    /// Copy-only projection reused while rendering floating branches.
    pub(in crate::layout) render_layout_scratch: Vec<LeafFrameInfo>,
}

impl<W: LayoutElement> WorkspaceRuntime<W> {
    pub(in crate::layout) fn new(view_size: Size<f64, Logical>, options: &Options) -> Self {
        let shadow_config =
            compute_workspace_shadow_config(options.overview.workspace_shadow, view_size);
        Self {
            tiled: TiledRuntime {
                interactive_resize: None,
                closing_windows: Vec::new(),
            },
            floating: FloatingRuntimeState {
                interactive_resize: None,
                closing_windows: Vec::new(),
                render_layout_scratch: Vec::new(),
            },
            floating_raised_at: None,
            is_active: false,
            shadow: Shadow::new(shadow_config),
            background: SolidColorBuffer::new(view_size, options.layout.background_color),
            overview_offscreen: OffscreenBuffer::default(),
        }
    }

    pub(in crate::layout) fn advance_closing_animations(&mut self) {
        for closing_windows in [
            &mut self.tiled.closing_windows,
            &mut self.floating.closing_windows,
        ] {
            closing_windows.retain_mut(|closing| {
                closing.advance_animations();
                closing.are_animations_ongoing()
            });
        }
    }

    pub(in crate::layout) fn has_closing_windows(&self) -> bool {
        !self.tiled.closing_windows.is_empty() || !self.floating.closing_windows.is_empty()
    }
}
