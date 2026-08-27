//! State owned exclusively by the tiled side of a workspace.

use super::closing_window::ClosingWindow;
use super::container::ResizeTarget;
use super::{InteractiveResizeData, LayoutElement};

/// Interaction and presentation state that has no floating counterpart.
///
/// Container nodes, including tabbed and stacked parents, stay in the workspace's shared
/// container tree. This type owns only state whose lifetime belongs to the tiled side itself.
#[derive(Debug)]
pub struct TilingSpace<W: LayoutElement> {
    pub(super) interactive_resize: Option<InteractiveResizeState<W>>,
    pub(super) closing_windows: Vec<ClosingWindow>,
}

impl<W: LayoutElement> TilingSpace<W> {
    pub(super) fn new() -> Self {
        Self {
            interactive_resize: None,
            closing_windows: Vec::new(),
        }
    }

    pub(super) fn advance_animations(&mut self) {
        self.closing_windows.retain_mut(|closing| {
            closing.advance_animations();
            closing.are_animations_ongoing()
        });
    }

    pub(super) fn are_animations_ongoing(&self) -> bool {
        !self.closing_windows.is_empty()
    }

    pub(super) fn are_transitions_ongoing(&self) -> bool {
        !self.closing_windows.is_empty()
    }
}

#[derive(Debug, Clone)]
pub(super) struct InteractiveResizeState<W: LayoutElement> {
    pub(super) window: W::Id,
    pub(super) data: InteractiveResizeData,
    pub(super) horizontal: Option<ResizeTarget>,
    pub(super) vertical: Option<ResizeTarget>,
}
