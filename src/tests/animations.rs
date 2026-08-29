use std::fmt::Write as _;
use std::time::Duration;

use insta::assert_snapshot;
use smithay::utils::{Point, Size};
use tiri_config::animations::{Curve, EasingParams, Kind};
use tiri_config::Config;
use tiri_ipc::SizeChange;
use wayland_client::protocol::wl_surface::WlSurface;

use super::client::ClientId;
use super::*;
use crate::layout::ContainerLayout;
use crate::tiri::Niri;

fn format_tiles(niri: &Niri) -> String {
    let mut buf = String::new();
    let (ws, containers) = niri.layout.active_workspace_and_containers().unwrap();
    let mut tiles: Vec<_> = ws.tiles_with_render_positions(containers).collect();

    // We sort by id since that gives us a consistent order (from first opened to last), but we
    // don't print the id since it's nondeterministic (the id is a global counter across all
    // running tests in the same binary).
    tiles.sort_by_key(|(tile, _, _)| tile.window().id().get());
    for (tile, pos, _visible) in tiles {
        let Size { w, h, .. } = tile.animated_tile_size();
        let Point { x, y, .. } = pos;
        writeln!(&mut buf, "{w:>3.0} × {h:>3.0} at x:{x:>3.0} y:{y:>3.0}").unwrap();
    }
    buf
}

fn create_window(f: &mut Fixture, id: ClientId, w: u16, h: u16) -> WlSurface {
    let window = f.client(id).create_window();
    let surface = window.surface.clone();
    window.commit();
    f.roundtrip(id);

    let window = f.client(id).window(&surface);
    window.attach_new_buffer();
    window.set_size(w, h);
    window.ack_last_and_commit();
    f.roundtrip(id);

    surface
}

fn set_time(niri: &mut Niri, time: Duration) {
    // This is a bit involved because we're dealing with an AdjustableClock that maintains its own
    // internal current_time.

    // First, reset current_time to zero by matching unadjusted time to it (at rate 0.0), then
    // setting unadjusted time to zero at rate 1.0 (causing current_time to also go to zero).
    let now = niri.clock.now();
    niri.clock.set_unadjusted(now);
    let _ = niri.clock.now();
    niri.clock.set_unadjusted(Duration::ZERO);
    niri.clock.set_rate(1.0);
    let _ = niri.clock.now();

    // Now, set the desired time at rate 1.0.
    niri.clock.set_unadjusted(time);
    let _ = niri.clock.now();

    // Freeze the clock so that clear() inside the niri loop callback followed by some get()
    // doesn't replace it with the monotonic time.
    niri.clock.set_rate(0.0);
}

// Sets up a fixture with linear animations, a renderer, and an output.
fn set_up() -> Fixture {
    const LINEAR: Kind = Kind::Easing(EasingParams {
        duration_ms: 1000,
        curve: Curve::Linear,
    });

    let mut config = Config::default();
    config.layout.gaps = 0.0;
    config.animations.window_resize.anim.kind = LINEAR;
    config.animations.window_movement.0.kind = LINEAR;

    let mut f = Fixture::with_config(config);
    f.niri_state().backend.headless().add_renderer().unwrap();
    f.add_output(1, (1920, 1080));

    f
}

fn set_up_two_in_column() -> (Fixture, ClientId, WlSurface, WlSurface) {
    let mut f = set_up();

    let id = f.add_client();

    let surface1 = create_window(&mut f, id, 100, 100);
    let surface2 = create_window(&mut f, id, 200, 200);
    f.double_roundtrip(id);

    let _ = f.client(id).window(&surface1).recent_configures();
    let _ = f.client(id).window(&surface2).recent_configures();

    // Consume into one column.
    f.niri().layout.focus_left();
    f.niri().layout.consume_into_column();
    f.niri().layout.set_layout_mode(ContainerLayout::SplitV);
    f.double_roundtrip(id);

    // Commit for the column consume.
    apply_recent_configure_if_any(&mut f, id, &surface1);
    apply_recent_configure_if_any(&mut f, id, &surface2);

    f.double_roundtrip(id);

    set_time(f.niri(), Duration::ZERO);
    f.niri_complete_animations();

    (f, id, surface1, surface2)
}

fn apply_recent_configure_if_any(f: &mut Fixture, id: ClientId, surface: &WlSurface) {
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

#[test]
fn egl_height_resize_animates_next_y() {
    let (mut f, id, surface1, surface2) = set_up_two_in_column();

    // Issue a resize.
    f.niri()
        .layout
        .set_window_height(None, SizeChange::AdjustFixed(-50));
    f.double_roundtrip(id);

    // Apply compositor configures for this resize.
    apply_recent_configure_if_any(&mut f, id, &surface1);
    apply_recent_configure_if_any(&mut f, id, &surface2);

    // This starts the resize animation for the top window and the Y move for the bottom.
    f.roundtrip(id);

    // No time had passed yet, so we're at the initial state.
    assert_snapshot!(format_tiles(f.niri()), @"100 × 100 at x:  0 y:  0");

    // Advance the time halfway.
    set_time(f.niri(), Duration::from_millis(500));
    f.niri().advance_animations();

    // Top window is half-resized at 75 px tall, bottom window is at y=75 matching it.
    assert_snapshot!(format_tiles(f.niri()), @"100 × 100 at x:  0 y:  0");

    // Advance the time to completion.
    set_time(f.niri(), Duration::from_millis(1000));
    f.niri().advance_animations();

    // Final state at 50 px.
    assert_snapshot!(format_tiles(f.niri()), @"100 × 100 at x:  0 y:  0");
}

#[test]
fn egl_clientside_height_change_doesnt_animate() {
    let (mut f, id, surface1, _surface2) = set_up_two_in_column();

    // The initial state.
    assert_snapshot!(format_tiles(f.niri()), @"100 × 100 at x:  0 y:  0");

    // The top window shrinks by itself, without a niri-issued resize.
    let window = f.client(id).window(&surface1);
    window.set_size(100, 50);
    window.commit();

    // This does not start any animations.
    f.roundtrip(id);

    // No time had passed yet, but we are at the final state right away.
    assert_snapshot!(format_tiles(f.niri()), @"100 × 100 at x:  0 y:  0");
}
