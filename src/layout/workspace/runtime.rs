//! Ephemeral interaction and rendering state for one workspace.
//!
//! Tree topology and semantic layout state do not live here. Keeping this module separate makes
//! that ownership boundary visible without storing runtime data inside the tree it operates on.

use crate::layout::closing_window::ClosingWindow;
use crate::layout::container::{InteractiveResizeState, LayoutLayer, NodeKey, SwapEffects};
use crate::layout::container_tree::ContainerTree;
use crate::layout::floating::FloatingRuntime;
use crate::layout::LayoutElement;

#[derive(Debug)]
pub(super) struct WorkspaceRuntime<W: LayoutElement> {
    pub(super) tiled: TiledRuntime<W>,
    pub(super) floating: FloatingRuntime<W>,
    /// Which branch owns focus, including the focus-without-raise render exception.
    pub(super) layer: WorkspaceLayerState,
    /// Whether this workspace owns compositor focus in the current render state.
    pub(super) is_active: bool,
}

#[derive(Debug)]
pub(super) struct TiledRuntime<W: LayoutElement> {
    /// Ongoing interactive resize. A window can only be resized in its current branch.
    pub(super) interactive_resize: Option<InteractiveResizeState<W>>,
    /// Windows in their closing animation, drawn in the tiled render pass.
    pub(super) closing_windows: Vec<ClosingWindow>,
}

impl<W: LayoutElement> WorkspaceRuntime<W> {
    pub(super) fn new(workspace: NodeKey) -> Self {
        Self {
            tiled: TiledRuntime {
                interactive_resize: None,
                closing_windows: Vec::new(),
            },
            floating: FloatingRuntime::new(workspace),
            layer: WorkspaceLayerState::Tiled,
            is_active: false,
        }
    }

    pub(super) fn layer_has_focus(&self, layer: LayoutLayer) -> bool {
        self.is_active && self.layer.layer() == layer
    }

    pub(super) fn apply_swap_effects(&mut self, tree: &ContainerTree<W>, effects: SwapEffects) {
        if effects.floating_ownership_changed && effects.affects(self.floating.workspace()) {
            self.floating.cancel_resize_if_target_left(tree);
        }
    }
}

/// Focus/render-layer state for the two branches of a workspace tree.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum WorkspaceLayerState {
    /// The tiled branch owns focus.
    Tiled,
    /// The tiled branch owns focus while floating remains raised for focus-follows-mouse.
    TiledWithFloatingRaised,
    /// The floating branch owns focus.
    Floating,
}

impl WorkspaceLayerState {
    pub(super) fn floating_has_focus(self) -> bool {
        self == Self::Floating
    }

    fn layer(self) -> LayoutLayer {
        if self.floating_has_focus() {
            LayoutLayer::Floating
        } else {
            LayoutLayer::Tiled
        }
    }
}
