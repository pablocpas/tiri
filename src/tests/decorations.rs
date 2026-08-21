//! What the border actually paints, read back off a rendered frame.
//!
//! The layout tests can only assert which state a tile resolved to. These render the output
//! and read the color back off the frame, because the decoration states are a visual feature
//! and this suite has been green while the screen was wrong before.

use client::ClientId;
use smithay::backend::allocator::Fourcc;
use smithay::utils::{Logical, Physical, Point, Scale, Size, Transform};
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

const TAB_ACTIVE_BG: [u8; 3] = [0x7f, 0xc8, 0xff];
const TAB_ACTIVE_RIM: [u8; 3] = [0x2e, 0x9e, 0xf4];
const TAB_INACTIVE_RIM: [u8; 3] = [0x6a, 0x6a, 0x6a];
const TAB_FOCUSED_INACTIVE_BG: [u8; 3] = [0x63, 0x8c, 0xab];
/// Deliberately nothing like the tab palette: these tests tell the two lanes apart by color.
const BORDER_IN_TABS: [u8; 3] = [0xff, 0x00, 0xff];

/// A window under a tab bar, with the border and the bar in colors that cannot be confused.
fn tabbed_config() -> Config {
    let color = |[r, g, b]: [u8; 3]| Color::from_rgba8_unpremul(r, g, b, 255);
    let mut config = palette_config();
    config.layout.gaps = 8.;
    config.layout.border.width = BORDER_WIDTH;
    config.layout.border.active_color = color(BORDER_IN_TABS);
    config.layout.tab_bar.active_bg = color(TAB_ACTIVE_BG);
    config.layout.tab_bar.focused_inactive_bg = color(TAB_FOCUSED_INACTIVE_BG);
    config.layout.tab_bar.border_width = 1.;
    config.layout.tab_bar.active_border = color(TAB_ACTIVE_RIM);
    config.layout.tab_bar.inactive_border = color(TAB_INACTIVE_RIM);

    // The fixture's windows keep their own decorations, which would make the border paint as
    // a rectangle behind the window instead of as the four lanes this is about.
    config.window_rules.push(tiri_config::WindowRule {
        draw_border_with_background: Some(false),
        ..Default::default()
    });
    config
}

/// The visible tile's position and size.
fn visible_tile(f: &mut Fixture) -> (Point<f64, Logical>, Size<f64, Logical>) {
    visible_tile_other_than(f, u64::MAX)
}

/// The visible tile that is not `skip`, for when a float is on screen too.
fn visible_tile_other_than(
    f: &mut Fixture,
    skip: u64,
) -> (Point<f64, Logical>, Size<f64, Logical>) {
    f.niri()
        .layout
        .active_workspace()
        .expect("active workspace")
        .tiles_with_render_positions()
        .find_map(|(tile, pos, visible)| {
            (visible && tile.window().id().get() != skip).then(|| (pos, tile.tile_size()))
        })
        .expect("a visible tile")
}

fn pixel(pixels: &[u8], size: Size<i32, Physical>, x: i32, y: i32) -> [u8; 3] {
    let idx = ((y * size.w + x) * 4) as usize;
    [pixels[idx], pixels[idx + 1], pixels[idx + 2]]
}

fn three_tabbed_windows(f: &mut Fixture, id: ClientId, surfaces: &mut Vec<WlSurface>) -> u64 {
    add_window(f, id, surfaces);
    add_window(f, id, surfaces);
    let last = add_window(f, id, surfaces);

    f.niri()
        .layout
        .set_layout_mode(crate::layout::ContainerLayout::Tabbed);
    // The tabbed arrange resizes every window; two rounds because the first one only gets
    // the configures out.
    settle(f, id, surfaces);
    settle(f, id, surfaces);
    last
}

/// i3's normal border style is a title bar on top and a border on the other three sides.
/// The tab is that title bar, so the lane between it and the window belongs to the bar —
/// painted in the selected tab's color — and the tile does not draw its own top border
/// there. Getting this wrong stacks two decorations, which is what it looked like.
#[test]
fn a_tab_takes_the_place_of_the_top_border_of_the_window_under_it() {
    let mut f = Fixture::with_config(tabbed_config());
    f.niri_state().backend.headless().add_renderer().unwrap();
    f.add_output(1, (1920, 1080));

    let id = f.add_client();
    let mut surfaces = Vec::new();
    three_tabbed_windows(&mut f, id, &mut surfaces);

    let (pos, tile) = visible_tile(&mut f);
    let (pixels, size) = render(&mut f);

    let x = (pos.x + tile.w / 2.).round() as i32;
    let lane = (pos.y + BORDER_WIDTH / 2.).round() as i32;
    assert_color(
        pixel(&pixels, size, x, lane),
        TAB_ACTIVE_BG,
        "the lane between the tabs and the window",
    );

    // The other three sides still carry the border, so this is the top edge being replaced,
    // not the border being turned off.
    let y = (pos.y + tile.h / 2.).round() as i32;
    let left = (pos.x + BORDER_WIDTH / 2.).round() as i32;
    assert_color(
        pixel(&pixels, size, left, y),
        BORDER_IN_TABS,
        "the left border of the window under the tabs",
    );
}

/// The selected tab of a container the seat is not in is `focused_inactive`, like the
/// window borders: it is where the focus would land on the way back in. Without the state
/// a tab bar says nothing about a container you are not standing in.
#[test]
fn the_selected_tab_of_a_container_without_the_focus_is_focused_inactive() {
    let mut f = Fixture::with_config(tabbed_config());
    f.niri_state().backend.headless().add_renderer().unwrap();
    f.add_output(1, (1920, 1080));

    let id = f.add_client();
    let mut surfaces = Vec::new();
    let floated = three_tabbed_windows(&mut f, id, &mut surfaces);

    let (pos, tile) = visible_tile(&mut f);
    let x = (pos.x + tile.w / 2.).round() as i32;
    let lane = (pos.y + BORDER_WIDTH / 2.).round() as i32;

    let (pixels, size) = render(&mut f);
    assert_color(
        pixel(&pixels, size, x, lane),
        TAB_ACTIVE_BG,
        "the selected tab while the container holds the focus",
    );

    // Float the selected window out: the focus leaves the tabbed container, which keeps
    // pointing at the tab it would come back to.
    f.niri().layout.toggle_window_floating(None);
    settle(&mut f, id, &surfaces);
    settle(&mut f, id, &surfaces);

    let (pos, tile) = visible_tile_other_than(&mut f, floated);
    let x = (pos.x + tile.w / 2.).round() as i32;
    let lane = (pos.y + BORDER_WIDTH / 2.).round() as i32;
    let (pixels, size) = render(&mut f);
    assert_color(
        pixel(&pixels, size, x, lane),
        TAB_FOCUSED_INACTIVE_BG,
        "the selected tab once the focus is on the float",
    );
}

/// The selected tab and the strip under the row are one shape — the top of the frame
/// around the window, the way i3 gives a title bar and its `child_border` one value. The
/// per-tab rim must not run along that seam, or it draws a line through the middle of it.
/// The tabs that stay closed keep theirs.
#[test]
fn the_selected_tab_runs_into_the_window_frame_without_a_seam() {
    let mut f = Fixture::with_config(tabbed_config());
    f.niri_state().backend.headless().add_renderer().unwrap();
    f.add_output(1, (1920, 1080));

    let id = f.add_client();
    let mut surfaces = Vec::new();
    three_tabbed_windows(&mut f, id, &mut surfaces);

    let (pos, tile) = visible_tile(&mut f);
    let (pixels, size) = render(&mut f);

    // The row of the bar that touches the strip. The third of three windows is the one
    // holding the focus, so it owns the last tab.
    let seam = (pos.y - 1.).round() as i32;
    let selected = (pos.x + tile.w * 5. / 6.).round() as i32;
    let closed = (pos.x + tile.w / 6.).round() as i32;

    assert_color(
        pixel(&pixels, size, selected, seam),
        TAB_ACTIVE_BG,
        "the bottom of the selected tab",
    );
    assert_color(
        pixel(&pixels, size, closed, seam),
        TAB_INACTIVE_RIM,
        "the bottom of a tab that is not selected",
    );

    // Stacked puts the tabs in rows; only the last one touches the strip, and here that is
    // the selected one again.
    f.niri()
        .layout
        .set_layout_mode(crate::layout::ContainerLayout::Stacked);
    settle(&mut f, id, &surfaces);
    settle(&mut f, id, &surfaces);

    let (pos, tile) = visible_tile(&mut f);
    let (pixels, size) = render(&mut f);
    let seam = (pos.y - 1.).round() as i32;
    assert_color(
        pixel(&pixels, size, (pos.x + tile.w / 2.).round() as i32, seam),
        TAB_ACTIVE_BG,
        "the bottom of the selected row, stacked",
    );
}
