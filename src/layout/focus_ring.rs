use std::iter::zip;

use smithay::backend::renderer::element::{Element as _, Kind};
use smithay::utils::{Logical, Point, Rectangle, Scale, Size};
use tiri_config::{CornerRadius, Gradient, GradientRelativeTo};

use crate::niri_render_elements;
use crate::render_helpers::border::BorderRenderElement;
use crate::render_helpers::renderer::NiriRenderer;
use crate::render_helpers::solid_color::{SolidColorBuffer, SolidColorRenderElement};
use crate::utils::round_logical_in_physical_max1;

/// Selects the visual style for container-selection highlight.
///
/// Prefer the configured focus ring when it is effectively visible; otherwise
/// fall back to border styling, which also carries focused/active colors.
pub fn container_selection_config(
    focus_ring: tiri_config::FocusRing,
    border: tiri_config::Border,
) -> tiri_config::FocusRing {
    if !focus_ring.off && focus_ring.width > 0.0 {
        focus_ring
    } else {
        border.into()
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct FocusRingEdges {
    pub top: bool,
    pub bottom: bool,
    pub left: bool,
    pub right: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FocusRingIndicatorEdge {
    Top,
    Bottom,
    Left,
    Right,
}

impl FocusRingEdges {
    pub const ALL: Self = Self {
        top: true,
        bottom: true,
        left: true,
        right: true,
    };
    pub const NONE: Self = Self {
        top: false,
        bottom: false,
        left: false,
        right: false,
    };

    pub fn all() -> Self {
        Self::ALL
    }

    pub fn none() -> Self {
        Self::NONE
    }
}

#[derive(Debug)]
pub struct FocusRing {
    buffers: [SolidColorBuffer; 8],
    locations: [Point<f64, Logical>; 8],
    sizes: [Size<f64, Logical>; 8],
    borders: [BorderRenderElement; 8],
    full_size: Size<f64, Logical>,
    is_border: bool,
    use_border_shader: bool,
    config: tiri_config::FocusRing,
    thicken_corners: bool,
    edges: FocusRingEdges,
}

niri_render_elements! {
    FocusRingRenderElement => {
        SolidColor = SolidColorRenderElement,
        Gradient = BorderRenderElement,
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FocusRingState {
    Focused,
    FocusedInactive,
    Unfocused,
    Urgent,
}

#[allow(clippy::too_many_arguments)]
pub fn render_container_selection<R: NiriRenderer>(
    renderer: &mut R,
    mut rect: Rectangle<f64, Logical>,
    mut clip_rect: Rectangle<f64, Logical>,
    scale: f64,
    is_active: bool,
    focus_ring: tiri_config::FocusRing,
    border: tiri_config::Border,
    push: &mut dyn FnMut(FocusRingRenderElement),
) {
    if !rect.overlaps(clip_rect) {
        return;
    }

    let mut ring_config = container_selection_config(focus_ring, border);
    ring_config.width = round_logical_in_physical_max1(scale, ring_config.width);
    let mut ring = FocusRing::new(ring_config);

    // Match tile rendering: align selection geometry to physical pixels.
    let output_scale = Scale::from(scale);
    rect.loc = rect
        .loc
        .to_physical_precise_round(output_scale)
        .to_logical(output_scale);
    rect.size = rect
        .size
        .to_physical_precise_round(output_scale)
        .to_logical(output_scale);
    clip_rect.loc = clip_rect
        .loc
        .to_physical_precise_round(output_scale)
        .to_logical(output_scale);
    clip_rect.size = clip_rect
        .size
        .to_physical_precise_round(output_scale)
        .to_logical(output_scale);

    // The lane lives inside the node's box.
    //
    // A selection rect is a container's box, and a container's box is flush with its
    // siblings': sway gives a floating container no inner gaps at all, and a workspace with
    // `gaps 0` none either. Anything painted outside the box therefore lands on the
    // neighbour, and which of the two you see is decided by the order the tree happens to
    // be walked in. Inside, it is the node's own to paint, always.
    //
    // This is the same lane the border occupies, which is what i3 draws for the state of a
    // node and where `container_selection_config` falls back when the ring is off.
    let width = ring.width();
    if width > 0.0 {
        let inset_x = width.min(rect.size.w / 2.0);
        let inset_y = width.min(rect.size.h / 2.0);
        rect.loc.x += inset_x;
        rect.loc.y += inset_y;
        rect.size.w = (rect.size.w - inset_x * 2.0).max(0.0);
        rect.size.h = (rect.size.h - inset_y * 2.0).max(0.0);
    }

    if rect.size.w <= 0.0 || rect.size.h <= 0.0 {
        return;
    }

    let ring_state = if is_active {
        FocusRingState::Focused
    } else {
        FocusRingState::FocusedInactive
    };
    ring.update_render_elements(
        rect.size,
        ring_state,
        true,
        FocusRingEdges::all(),
        None,
        Rectangle::new(clip_rect.loc - rect.loc, clip_rect.size),
        tiri_config::CornerRadius::default(),
        scale,
        1.0,
    );
    ring.render(renderer, rect.loc, &mut |elem| push(elem));
}

impl FocusRing {
    pub fn new(config: tiri_config::FocusRing) -> Self {
        Self {
            buffers: Default::default(),
            locations: Default::default(),
            sizes: Default::default(),
            borders: Default::default(),
            full_size: Default::default(),
            is_border: false,
            use_border_shader: false,
            config,
            thicken_corners: true,
            edges: FocusRingEdges::all(),
        }
    }

    pub fn update_config(&mut self, config: tiri_config::FocusRing) {
        self.config = config;
    }

    pub fn update_shaders(&mut self) {
        for elem in &mut self.borders {
            elem.damage_all();
        }
    }

    #[allow(clippy::too_many_arguments)]
    pub fn update_render_elements(
        &mut self,
        win_size: Size<f64, Logical>,
        state: FocusRingState,
        is_border: bool,
        edges: FocusRingEdges,
        indicator_edge: Option<FocusRingIndicatorEdge>,
        view_rect: Rectangle<f64, Logical>,
        radius: CornerRadius,
        scale: f64,
        alpha: f32,
    ) {
        let width = self.config.width;
        self.full_size = win_size + Size::from((width, width)).upscale(2.);
        self.is_border = is_border;
        self.edges = edges;

        let (color, gradient, indicator_color, indicator_gradient) = match state {
            FocusRingState::Urgent => (
                self.config.urgent_color,
                self.config.urgent_gradient,
                self.config.urgent_indicator_color,
                self.config.urgent_indicator_gradient,
            ),
            FocusRingState::Focused => (
                self.config.active_color,
                self.config.active_gradient,
                self.config.active_indicator_color,
                self.config.active_indicator_gradient,
            ),
            FocusRingState::FocusedInactive => (
                self.config.focused_inactive_color,
                self.config
                    .focused_inactive_gradient
                    .or(self.config.inactive_gradient),
                self.config.focused_inactive_indicator_color,
                self.config
                    .focused_inactive_indicator_gradient
                    .or(self.config.inactive_indicator_gradient),
            ),
            FocusRingState::Unfocused => (
                self.config.inactive_color,
                self.config.inactive_gradient,
                self.config.inactive_indicator_color,
                self.config.inactive_indicator_gradient,
            ),
        };

        let indicator_edge = if is_border { indicator_edge } else { None };
        let is_indicator_segment = |idx| match indicator_edge {
            Some(FocusRingIndicatorEdge::Top) => idx == 0,
            Some(FocusRingIndicatorEdge::Bottom) => idx == 1,
            Some(FocusRingIndicatorEdge::Left) => idx == 2,
            Some(FocusRingIndicatorEdge::Right) => idx == 3,
            None => false,
        };

        for (idx, buf) in self.buffers.iter_mut().enumerate() {
            let segment_color = if is_indicator_segment(idx) {
                indicator_color
            } else {
                color
            };
            buf.set_color(segment_color);
        }

        let radius = radius.fit_to(self.full_size.w as f32, self.full_size.h as f32);

        self.use_border_shader =
            radius != CornerRadius::default() || gradient.is_some() || indicator_gradient.is_some();

        // Set the defaults for solid color + rounded corners.
        let base_gradient = gradient.unwrap_or_else(|| Gradient::from(color));
        let indicator_gradient =
            indicator_gradient.unwrap_or_else(|| Gradient::from(indicator_color));

        let full_rect = Rectangle::new(Point::from((-width, -width)), self.full_size);
        let base_gradient_area = match base_gradient.relative_to {
            GradientRelativeTo::Window => full_rect,
            GradientRelativeTo::WorkspaceView => view_rect,
        };
        let indicator_gradient_area = match indicator_gradient.relative_to {
            GradientRelativeTo::Window => full_rect,
            GradientRelativeTo::WorkspaceView => view_rect,
        };

        let rounded_corner_border_width = if is_border {
            // HACK: increase the border width used for the inner rounded corners a tiny bit to
            // reduce background bleed.
            let extra = if self.thicken_corners { 0.5 } else { 0. };
            width as f32 + extra
        } else {
            0.
        };

        let ceil = |logical: f64| (logical * scale).ceil() / scale;

        // All of this stuff should end up aligned to physical pixels because:
        // * Window size and border width are rounded to physical pixels before being passed to this
        //   function.
        // * We will ceil the corner radii below.
        // * We do not divide anything, only add, subtract and multiply by integers.
        // * At rendering time, tile positions are rounded to physical pixels.

        if is_border {
            let top_left = f64::max(width, ceil(f64::from(radius.top_left)));
            let top_right = f64::min(
                self.full_size.w - top_left,
                f64::max(width, ceil(f64::from(radius.top_right))),
            );
            let bottom_left = f64::min(
                self.full_size.h - top_left,
                f64::max(width, ceil(f64::from(radius.bottom_left))),
            );
            let bottom_right = f64::min(
                self.full_size.h - top_right,
                f64::min(
                    self.full_size.w - bottom_left,
                    f64::max(width, ceil(f64::from(radius.bottom_right))),
                ),
            );

            // Top edge.
            self.sizes[0] = Size::from((win_size.w + width * 2. - top_left - top_right, width));
            self.locations[0] = Point::from((-width + top_left, -width));

            // Bottom edge.
            self.sizes[1] =
                Size::from((win_size.w + width * 2. - bottom_left - bottom_right, width));
            self.locations[1] = Point::from((-width + bottom_left, win_size.h));

            // Left edge.
            self.sizes[2] = Size::from((width, win_size.h + width * 2. - top_left - bottom_left));
            self.locations[2] = Point::from((-width, -width + top_left));

            // Right edge.
            self.sizes[3] = Size::from((width, win_size.h + width * 2. - top_right - bottom_right));
            self.locations[3] = Point::from((win_size.w, -width + top_right));

            // Top-left corner.
            self.sizes[4] = Size::from((top_left, top_left));
            self.locations[4] = Point::from((-width, -width));

            // Top-right corner.
            self.sizes[5] = Size::from((top_right, top_right));
            self.locations[5] = Point::from((win_size.w + width - top_right, -width));

            // Bottom-right corner.
            self.sizes[6] = Size::from((bottom_right, bottom_right));
            self.locations[6] = Point::from((
                win_size.w + width - bottom_right,
                win_size.h + width - bottom_right,
            ));

            // Bottom-left corner.
            self.sizes[7] = Size::from((bottom_left, bottom_left));
            self.locations[7] = Point::from((-width, win_size.h + width - bottom_left));

            for (buf, size) in zip(&mut self.buffers, self.sizes) {
                buf.resize(size);
            }

            for (idx, (border, (loc, size))) in
                zip(&mut self.borders, zip(self.locations, self.sizes)).enumerate()
            {
                let (gradient, gradient_area) = if is_indicator_segment(idx) {
                    (&indicator_gradient, indicator_gradient_area)
                } else {
                    (&base_gradient, base_gradient_area)
                };
                border.update(
                    size,
                    Rectangle::new(gradient_area.loc - loc, gradient_area.size),
                    gradient.in_,
                    gradient.from,
                    gradient.to,
                    ((gradient.angle as f32) - 90.).to_radians(),
                    Rectangle::new(full_rect.loc - loc, full_rect.size),
                    rounded_corner_border_width,
                    radius,
                    scale as f32,
                    alpha,
                );
            }
        } else {
            self.sizes[0] = self.full_size;
            self.buffers[0].resize(self.sizes[0]);
            self.locations[0] = Point::from((-width, -width));

            self.borders[0].update(
                self.sizes[0],
                Rectangle::new(
                    base_gradient_area.loc - self.locations[0],
                    base_gradient_area.size,
                ),
                base_gradient.in_,
                base_gradient.from,
                base_gradient.to,
                ((base_gradient.angle as f32) - 90.).to_radians(),
                Rectangle::new(full_rect.loc - self.locations[0], full_rect.size),
                rounded_corner_border_width,
                radius,
                scale as f32,
                alpha,
            );
        }
    }

    pub fn render(
        &self,
        renderer: &mut impl NiriRenderer,
        location: Point<f64, Logical>,
        push: &mut dyn FnMut(FocusRingRenderElement),
    ) {
        if self.config.off {
            return;
        }

        let border_width = -self.locations[0].y;

        // If drawing as a border with width = 0, then there's nothing to draw.
        if self.is_border && border_width == 0. {
            return;
        }

        let has_border_shader = BorderRenderElement::has_shader(renderer);

        let mut push = |buffer, border: &BorderRenderElement, location: Point<f64, Logical>| {
            let elem = if self.use_border_shader && has_border_shader {
                border.clone().with_location(location).into()
            } else {
                let alpha = border.alpha();
                SolidColorRenderElement::from_buffer(buffer, location, alpha, Kind::Unspecified)
                    .into()
            };
            push(elem);
        };

        if self.is_border {
            let edges = self.edges;
            let corner_visible = |top: bool, left: bool| top && left;
            for (idx, ((buf, border), loc)) in
                zip(zip(&self.buffers, &self.borders), self.locations).enumerate()
            {
                let visible = match idx {
                    0 => edges.top,
                    1 => edges.bottom,
                    2 => edges.left,
                    3 => edges.right,
                    4 => corner_visible(edges.top, edges.left),
                    5 => corner_visible(edges.top, edges.right),
                    6 => corner_visible(edges.bottom, edges.right),
                    7 => corner_visible(edges.bottom, edges.left),
                    _ => true,
                };
                if !visible {
                    continue;
                }
                push(buf, border, location + loc);
            }
        } else {
            if self.edges != FocusRingEdges::none() {
                push(
                    &self.buffers[0],
                    &self.borders[0],
                    location + self.locations[0],
                );
            }
        }
    }

    pub fn width(&self) -> f64 {
        self.config.width
    }

    pub fn is_off(&self) -> bool {
        self.config.off
    }

    pub fn set_thicken_corners(&mut self, value: bool) {
        self.thicken_corners = value;
    }

    pub fn config(&self) -> &tiri_config::FocusRing {
        &self.config
    }

    /// The color the last update resolved, so a test can assert what is on screen rather
    /// than which branch was taken to get there.
    #[cfg(test)]
    pub fn debug_color(&self) -> smithay::backend::renderer::Color32F {
        self.buffers[0].color()
    }
}

#[cfg(test)]
mod tests {
    use super::container_selection_config;

    #[test]
    fn container_selection_prefers_focus_ring_when_visible() {
        let focus = tiri_config::FocusRing {
            off: false,
            width: 3.0,
            ..Default::default()
        };

        let border = tiri_config::Border::default();
        let selected = container_selection_config(focus, border);
        assert_eq!(selected, focus);
    }

    #[test]
    fn container_selection_falls_back_to_border_when_focus_ring_off() {
        let focus = tiri_config::FocusRing {
            off: true,
            width: 3.0,
            ..Default::default()
        };

        let border = tiri_config::Border {
            off: false,
            ..Default::default()
        };

        let selected = container_selection_config(focus, border);
        assert_eq!(selected, tiri_config::FocusRing::from(border));
    }

    #[test]
    fn container_selection_falls_back_to_border_when_focus_ring_zero_width() {
        let focus = tiri_config::FocusRing {
            off: false,
            width: 0.0,
            ..Default::default()
        };

        let border = tiri_config::Border {
            off: false,
            ..Default::default()
        };

        let selected = container_selection_config(focus, border);
        assert_eq!(selected, tiri_config::FocusRing::from(border));
    }
}
