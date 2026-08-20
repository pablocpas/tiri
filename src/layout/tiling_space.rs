//! State owned exclusively by the tiled side of a workspace.

use super::closing_window::ClosingWindow;
use super::container::ResizeTarget;
use super::viewport::FixedViewport;
use super::{InteractiveResizeData, LayoutElement};
use std::time::Duration;

/// Interaction and presentation state that has no floating counterpart.
///
/// Container nodes, including tabbed and stacked parents, stay in the workspace's shared
/// container tree. This type owns only state whose lifetime belongs to the tiled side itself.
#[derive(Debug)]
pub struct TilingSpace<W: LayoutElement> {
    /// Fixed for i3/sway tiling; retained to isolate the remaining viewport gesture API.
    pub(super) viewport: FixedViewport,
    pub(super) interactive_resize: Option<InteractiveResizeState<W>>,
    pub(super) closing_windows: Vec<ClosingWindow>,
}

impl<W: LayoutElement> TilingSpace<W> {
    pub(super) fn new() -> Self {
        Self {
            viewport: FixedViewport,
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

    pub(super) fn activation_view_distance(&self) -> f64 {
        self.viewport.activation_distance()
    }

    pub(super) fn horizontal_view_gesture_begin(&mut self, is_touchpad: bool) {
        self.viewport.begin_horizontal_gesture(is_touchpad);
    }

    pub(super) fn horizontal_view_gesture_update(
        &mut self,
        delta: f64,
        timestamp: Duration,
        is_touchpad: bool,
    ) -> Option<bool> {
        self.viewport
            .update_horizontal_gesture(delta, timestamp, is_touchpad)
    }

    pub(super) fn horizontal_view_gesture_end(&mut self, cancelled: Option<bool>) -> bool {
        self.viewport.end_horizontal_gesture(cancelled)
    }

    #[cfg(test)]
    pub(crate) fn view_pos(&self) -> f64 {
        self.viewport.position()
    }
}

#[derive(Debug, Clone)]
pub(super) struct InteractiveResizeState<W: LayoutElement> {
    pub(super) window: W::Id,
    pub(super) data: InteractiveResizeData,
    pub(super) horizontal: Option<ResizeTarget>,
    pub(super) vertical: Option<ResizeTarget>,
}
