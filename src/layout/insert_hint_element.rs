use smithay::utils::{Logical, Point, Rectangle, Size};
use tiri_config::CornerRadius;

use super::focus_ring::{FocusRing, FocusRingEdges, FocusRingRenderElement, FocusRingState};
use crate::render_helpers::renderer::NiriRenderer;

#[derive(Debug)]
pub struct InsertHintElement {
    inner: FocusRing,
}

pub type InsertHintRenderElement = FocusRingRenderElement;

impl InsertHintElement {
    fn focus_ring_config(config: tiri_config::InsertHint) -> tiri_config::FocusRing {
        tiri_config::FocusRing {
            off: config.off,
            width: 0.,
            active_color: config.color,
            focused_inactive_color: config.color,
            inactive_color: config.color,
            urgent_color: config.color,
            active_indicator_color: config.color,
            focused_inactive_indicator_color: config.color,
            inactive_indicator_color: config.color,
            urgent_indicator_color: config.color,
            active_gradient: config.gradient,
            active_indicator_gradient: config.gradient,
            focused_inactive_gradient: config.gradient,
            focused_inactive_indicator_gradient: config.gradient,
            inactive_gradient: config.gradient,
            inactive_indicator_gradient: config.gradient,
            urgent_gradient: config.gradient,
            urgent_indicator_gradient: config.gradient,
        }
    }

    pub fn new(config: tiri_config::InsertHint) -> Self {
        Self {
            inner: FocusRing::new(Self::focus_ring_config(config)),
        }
    }

    pub fn update_config(&mut self, config: tiri_config::InsertHint) {
        self.inner.update_config(Self::focus_ring_config(config));
    }

    pub fn update_shaders(&mut self) {
        self.inner.update_shaders();
    }

    pub fn update_render_elements(
        &mut self,
        size: Size<f64, Logical>,
        view_rect: Rectangle<f64, Logical>,
        radius: CornerRadius,
        scale: f64,
    ) {
        self.inner.update_render_elements(
            size,
            FocusRingState::Focused,
            false,
            FocusRingEdges::all(),
            None,
            view_rect,
            radius,
            scale,
            1.,
        );
    }

    pub fn render(
        &self,
        renderer: &mut impl NiriRenderer,
        location: Point<f64, Logical>,
        push: &mut dyn FnMut(FocusRingRenderElement),
    ) {
        self.inner.render(renderer, location, push)
    }
}
