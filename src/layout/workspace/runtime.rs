//! Ephemeral interaction and rendering state for one workspace.
//!
//! Tree topology and semantic layout state do not live here. Keeping this module separate makes
//! that ownership boundary visible without storing runtime data inside the tree it operates on.

use crate::layout::closing_window::ClosingWindow;
use crate::layout::container::{InteractiveResizeState, NodeKey, SwapEffects};
use crate::layout::container_tree::ContainerTree;
use crate::layout::floating::FloatingRuntime;
use crate::layout::LayoutElement;

#[derive(Debug)]
pub(super) struct WorkspaceRuntime<W: LayoutElement> {
    pub(super) tiled: TiledRuntime<W>,
    pub(super) floating: FloatingRuntime<W>,
    /// The seat generation at which focus moved to the tiled side without lowering the
    /// floating one.
    ///
    /// Which side has focus is the seat's to say (see [`ContainerTree::focused_layer`]). This
    /// is the one thing it cannot: focus-follows-mouse onto a tiled window leaves the floats
    /// drawn above a tiled fullscreen until focus next changes, and the generation is how
    /// "next" is noticed without every focus change having to clear it.
    pub(super) floating_raised_at: Option<u64>,
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
            floating_raised_at: None,
            is_active: false,
        }
    }

    pub(super) fn apply_swap_effects(&mut self, tree: &ContainerTree<W>, effects: SwapEffects) {
        if effects.floating_ownership_changed && effects.affects(self.floating.workspace()) {
            self.floating.cancel_resize_if_target_left(tree);
        }
    }
}
