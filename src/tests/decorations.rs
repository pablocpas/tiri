//! What the border actually paints, read back off a rendered frame.
//!
//! The layout tests can only assert which state a tile resolved to. These render the output
//! and read the color back off the frame, because the decoration states are a visual feature
//! and this suite has been green while the screen was wrong before.

use client::ClientId;
use smithay::backend::allocator::Fourcc;
use smithay::utils::{Physical, Scale, Size, Transform};
use tiri_config::{Color, Config};
use wayland_client::protocol::wl_surface::WlSurface;

use super::*;
use crate::render_helpers::{render_to_vec, RenderCtx, RenderTarget};

const BORDER_WIDTH: f64 = 6.;

const ACTIVE: [u8; 3] = [0x28, 0x55, 0x77];
const FOCUSED_INACTIVE: [u8; 3] = [0x5f, 0x67, 0x6a];
const INACTIVE: [u8; 3] = [0x22, 0x22, 0x22];

fn palette_config() -> Config {
    let color = |[r, g, b]: [u8; 3]| Color::from_rgba8_unpremul(r, g, b, 255);
    let mut config = Config::default();
    config.layout.gaps = 0.;
    config.layout.focus_ring.off = true;
    config.layout.border.off = false;
    config.layout.border.width = BORDER_WIDTH;
    config.layout.border.active_color = color(ACTIVE);
    config.layout.border.focused_inactive_color = color(FOCUSED_INACTIVE);
    config.layout.border.inactive_color = color(INACTIVE);
    config
}

fn set_up() -> Fixture {
    let mut f = Fixture::with_config(palette_config());
    f.niri_state().backend.headless().add_renderer().unwrap();
    f.add_output(1, (1920, 1080));
    f
}

/// Ack whatever the compositor last asked of every window, so the layout transaction the
/// previous command opened can complete. Without it the tree keeps its pending layout and
/// the tiles render at their old geometry — or, for a window still waiting on its first
/// configure, not at all.
fn settle(f: &mut Fixture, id: ClientId, surfaces: &[WlSurface]) {
    for surface in surfaces {
        let configure_size = {
            let window = f.client(id).window(surface);
            window.recent_configures().last().map(|c| c.size)
        };

        let window = f.client(id).window(surface);
        if let Some((w, h)) = configure_size {
            if let (Ok(w), Ok(h)) = (u16::try_from(w), u16::try_from(h)) {
                if w > 0 && h > 0 {
                    window.set_size(w, h);
                }
            }
            window.ack_last();
        }
        window.commit();
    }

    f.double_roundtrip(id);
    f.niri_complete_animations();
}

/// Map a window; the new one is the active window, which is where its id comes from.
fn add_window(f: &mut Fixture, id: ClientId, surfaces: &mut Vec<WlSurface>) -> u64 {
    let window = f.client(id).create_window();
    let surface = window.surface.clone();
    window.commit();
    f.roundtrip(id);

    let window = f.client(id).window(&surface);
    window.attach_new_buffer();
    window.set_size(200, 200);
    window.ack_last_and_commit();
    f.double_roundtrip(id);

    surfaces.push(surface);
    settle(f, id, surfaces);

    f.niri()
        .layout
        .active_workspace()
        .expect("active workspace")
        .active_window()
        .expect("active window")
        .id()
        .get()
}

/// The rendered output, as rows of RGBA.
fn render(f: &mut Fixture) -> (Vec<u8>, Size<i32, Physical>) {
    let output = f.niri_output(1);
    let size = output.current_mode().unwrap().size;
    let scale = Scale::from(output.current_scale().fractional_scale());

    let state = f.niri_state();
    state.niri.update_render_elements(Some(&output));
    let pixels = state
        .backend
        .with_primary_renderer(|renderer| {
            let ctx = RenderCtx {
                renderer,
                target: RenderTarget::Output,
                xray: None,
            };
            let elements = state.niri.render_to_vec(ctx, &output, false);
            render_to_vec(
                renderer,
                size,
                scale,
                Transform::Normal,
                Fourcc::Abgr8888,
                elements.iter().rev(),
            )
            .expect("render")
        })
        .expect("the fixture must have a renderer");

    (pixels, size)
}

/// The border color of window `window_id`.
///
/// Sampled inside the tile rather than in a border lane: the fixture's windows carry a fully
/// transparent buffer and the border is painted as the rectangle *behind* the window (what
/// `draw-border-with-background` does for a client that keeps its own decorations), so the
/// tile shows the border color throughout. The lanes are a worse place to look — one of the
/// four carries the split indicator, in its own color. Near the top edge, and clear of a
/// centered float lying over the middle of the tiles behind it.
fn border_color(
    f: &mut Fixture,
    window_id: u64,
    pixels: &[u8],
    size: Size<i32, Physical>,
) -> [u8; 3] {
    let (pos, tile_size) = f
        .niri()
        .layout
        .active_workspace()
        .expect("active workspace")
        .tiles_with_render_positions()
        .find_map(|(tile, pos, visible)| {
            (visible && tile.window().id().get() == window_id).then(|| (pos, tile.tile_size()))
        })
        .expect("the window must have a visible tile");

    let x = (pos.x + tile_size.w / 2.).round() as i32;
    let y = (pos.y + BORDER_WIDTH + 12.).round() as i32;
    let idx = ((y * size.w + x) * 4) as usize;
    [pixels[idx], pixels[idx + 1], pixels[idx + 2]]
}

#[track_caller]
fn assert_color(actual: [u8; 3], expected: [u8; 3], what: &str) {
    let close = (0..3).all(|i| actual[i].abs_diff(expected[i]) <= 2);
    assert!(
        close,
        "{what}: painted #{:02x}{:02x}{:02x}, expected #{:02x}{:02x}{:02x}",
        actual[0], actual[1], actual[2], expected[0], expected[1], expected[2],
    );
}

/// The state this was all for: focus a float, and the tiled window the workspace would come
/// back to keeps a distinct, dimmer border while its sibling drops to unfocused.
#[test]
fn focusing_a_float_paints_the_tiled_focus_head_focused_inactive() {
    let mut f = set_up();
    let id = f.add_client();
    let mut surfaces = Vec::new();

    let first = add_window(&mut f, id, &mut surfaces);
    let second = add_window(&mut f, id, &mut surfaces);
    let float = add_window(&mut f, id, &mut surfaces);

    f.niri().layout.toggle_window_floating(None);
    settle(&mut f, id, &surfaces);

    let (pixels, size) = render(&mut f);
    assert_color(
        border_color(&mut f, float, &pixels, size),
        ACTIVE,
        "the focused float",
    );
    assert_color(
        border_color(&mut f, second, &pixels, size),
        FOCUSED_INACTIVE,
        "the tiled window the workspace would return to",
    );
    assert_color(
        border_color(&mut f, first, &pixels, size),
        INACTIVE,
        "the tiled window nothing points at",
    );
}

/// Tiled siblings on their own: only the focused one is lit, because at the workspace level
/// the focus head *is* the focused window.
#[test]
fn tiled_siblings_of_the_focused_window_are_unfocused() {
    let mut f = set_up();
    let id = f.add_client();
    let mut surfaces = Vec::new();

    let first = add_window(&mut f, id, &mut surfaces);
    let second = add_window(&mut f, id, &mut surfaces);

    let (pixels, size) = render(&mut f);
    assert_color(
        border_color(&mut f, second, &pixels, size),
        ACTIVE,
        "the focused window",
    );
    assert_color(
        border_color(&mut f, first, &pixels, size),
        INACTIVE,
        "its sibling",
    );
}
