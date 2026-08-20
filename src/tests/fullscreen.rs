use client::ClientId;
use insta::assert_snapshot;
use smithay::utils::Point;
use wayland_client::protocol::wl_surface::WlSurface;

use super::*;
use crate::layout::LayoutElement as _;

// Sets up a fixture with two outputs and 100×100 window.
fn set_up() -> (Fixture, ClientId, WlSurface) {
    let mut f = Fixture::new();
    f.add_output(1, (1920, 1080));
    f.add_output(2, (1280, 720));

    let id = f.add_client();
    let window = f.client(id).create_window();
    let surface = window.surface.clone();
    window.commit();
    f.roundtrip(id);

    let window = f.client(id).window(&surface);
    window.attach_new_buffer();
    window.set_size(100, 100);
    window.ack_last_and_commit();
    f.double_roundtrip(id);

    (f, id, surface)
}

#[test]
fn fullscreen_binding_accepts_a_selected_floating_container() {
    let (mut f, id, _surface1) = set_up();

    let window2 = f.client(id).create_window();
    let surface2 = window2.surface.clone();
    window2.commit();
    f.roundtrip(id);

    let window2 = f.client(id).window(&surface2);
    window2.attach_new_buffer();
    window2.set_size(100, 100);
    window2.ack_last_and_commit();
    f.double_roundtrip(id);

    {
        let niri = f.niri();
        niri.layout.focus_parent();
        niri.layout.toggle_window_floating(None);
        assert!(niri.layout.active_selection_is_container());
    }

    // Exercise the same entrypoint as `Mod+F { fullscreen-window; }`, including its target
    // acceptance gate. Calling the layout method directly would miss the runtime regression.
    f.niri_state()
        .do_action(tiri_config::Action::FullscreenWindow, false);

    let workspace = f
        .niri()
        .layout
        .active_workspace()
        .expect("active workspace");
    assert_eq!(workspace.fullscreen_window_ids().len(), 1);
    assert!(workspace.render_above_top_layer());
    assert_eq!(
        workspace
            .tiles_with_render_positions()
            .filter(|(_, _, visible)| *visible)
            .count(),
        2,
        "both descendants of the fullscreen container must remain visible",
    );
    assert!(
        f.niri()
            .layout
            .windows()
            .all(|(_, window)| !window.pending_sizing_mode().is_fullscreen()),
        "container fullscreen must not be downgraded to client fullscreen on one window",
    );
}

#[test]
fn windowed_fullscreen() {
    let (mut f, id, surface) = set_up();

    let _ = f.client(id).window(&surface).recent_configures();

    let niri = f.niri();
    let mapped = niri.layout.windows().next().unwrap().1;
    let window_id = mapped.window.clone();

    // Legacy entrypoint now maps to real fullscreen.
    niri.layout.toggle_windowed_fullscreen(&window_id);
    f.double_roundtrip(id);

    // Should request real fullscreen.
    let window = f.client(id).window(&surface);
    assert_snapshot!(
        window.format_recent_configures(),
        @"size: 1920 × 1080, bounds: 1920 × 1080, states: [Activated, Fullscreen]"
    );

    let mapped = f.niri().layout.windows().next().unwrap().1;
    // The legacy entrypoint no longer flips a separate client-side mode.
    assert!(!mapped.is_windowed_fullscreen());

    // Commit in response.
    let window = f.client(id).window(&surface);
    window.ack_last_and_commit();
    f.roundtrip(id);

    let mapped = f.niri().layout.windows().next().unwrap().1;
    assert!(mapped.sizing_mode().is_fullscreen());
    assert!(!mapped.is_windowed_fullscreen());

    // Disable fullscreen.
    f.niri().layout.toggle_windowed_fullscreen(&window_id);
    f.double_roundtrip(id);

    // Should request without fullscreen state with the tiled size.
    let window = f.client(id).window(&surface);
    assert_snapshot!(
        window.format_recent_configures(),
        @"size: 1904 × 1064, bounds: 1904 × 1064, states: [Activated]"
    );

    let mapped = f.niri().layout.windows().next().unwrap().1;
    assert!(!mapped.is_windowed_fullscreen());

    // Commit in response.
    let window = f.client(id).window(&surface);
    window.ack_last_and_commit();
    f.roundtrip(id);

    let mapped = f.niri().layout.windows().next().unwrap().1;
    assert!(!mapped.sizing_mode().is_fullscreen());
    assert!(!mapped.is_windowed_fullscreen());
}

#[test]
fn windowed_fullscreen_chain() {
    let (mut f, id, surface) = set_up();

    let _ = f.client(id).window(&surface).recent_configures();

    let mapped = f.niri().layout.windows().next().unwrap().1;
    let window_id = mapped.window.clone();

    f.niri().layout.toggle_windowed_fullscreen(&window_id);
    f.roundtrip(id);
    f.niri().layout.toggle_windowed_fullscreen(&window_id);
    f.roundtrip(id);
    f.niri().layout.toggle_windowed_fullscreen(&window_id);
    f.roundtrip(id);
    f.niri().layout.toggle_windowed_fullscreen(&window_id);
    f.double_roundtrip(id);

    // Should be four configures matching the four requests.
    let window = f.client(id).window(&surface);
    assert_snapshot!(
        window.format_recent_configures(),
        @"
    size: 1920 × 1080, bounds: 1920 × 1080, states: [Activated, Fullscreen]
    size: 1920 × 1080, bounds: 1904 × 1064, states: [Activated]
    size: 1920 × 1080, bounds: 1920 × 1080, states: [Activated, Fullscreen]
    size: 1920 × 1080, bounds: 1904 × 1064, states: [Activated]
    "
    );

    let window = f.client(id).window(&surface);
    let serials = Vec::from_iter(
        window.configures_received[window.configures_received.len() - 4..]
            .iter()
            .map(|(s, _c)| *s),
    );

    let get_state = |f: &mut Fixture| {
        let mapped = f.niri().layout.windows().next().unwrap().1;
        format!(
            "fs {}, wfs {}",
            mapped.sizing_mode().is_fullscreen(),
            mapped.is_windowed_fullscreen()
        )
    };

    let mut states = vec![get_state(&mut f)];
    for serial in serials {
        let window = f.client(id).window(&surface);
        window.xdg_surface.ack_configure(serial);
        window.commit();
        f.roundtrip(id);
        states.push(get_state(&mut f));
    }

    // The legacy entrypoint now aliases real fullscreen.
    assert_snapshot!(
        states.join("\n"),
        @"
    fs false, wfs false
    fs true, wfs false
    fs false, wfs false
    fs true, wfs false
    fs false, wfs false
    "
    );
}

#[test]
fn client_fullscreen_request_uses_client_fullscreen_path() {
    let (mut f, id, surface) = set_up();

    let _ = f.client(id).window(&surface).recent_configures();

    // Client requests fullscreen via xdg-toplevel.
    f.client(id).window(&surface).set_fullscreen(None);
    f.double_roundtrip(id);

    // Client fullscreen should keep the window on the client-fullscreen path.
    let window = f.client(id).window(&surface);
    assert_snapshot!(
        window.format_recent_configures(),
        @"size: 1904 × 1064, bounds: 1920 × 1080, states: [Activated, Fullscreen]"
    );

    let mapped = f.niri().layout.windows().next().unwrap().1;
    assert!(!mapped.is_windowed_fullscreen());

    // Commit client fullscreen configure.
    let window = f.client(id).window(&surface);
    window.ack_last_and_commit();
    f.roundtrip(id);

    let mapped = f.niri().layout.windows().next().unwrap().1;
    assert!(!mapped.sizing_mode().is_fullscreen());
    assert!(mapped.is_windowed_fullscreen());

    // Client exits fullscreen.
    f.client(id).window(&surface).unset_fullscreen();
    f.double_roundtrip(id);

    let window = f.client(id).window(&surface);
    assert_snapshot!(
        window.format_recent_configures(),
        @"
    size: 1920 × 1080, bounds: 1920 × 1080, states: [Activated, Fullscreen]
    size: 1904 × 1064, bounds: 1904 × 1064, states: [Activated]
    "
    );

    let mapped = f.niri().layout.windows().next().unwrap().1;
    assert!(mapped.is_windowed_fullscreen());

    let window = f.client(id).window(&surface);
    window.ack_last_and_commit();
    f.roundtrip(id);

    let mapped = f.niri().layout.windows().next().unwrap().1;
    assert!(!mapped.sizing_mode().is_fullscreen());
    assert!(!mapped.is_windowed_fullscreen());
}

#[test]
fn client_fullscreen_request_reconfigures_while_wm_fullscreen_is_active() {
    let (mut f, id, surface) = set_up();

    let _ = f.client(id).window(&surface).recent_configures();

    let niri = f.niri();
    let mapped = niri.layout.windows().next().unwrap().1;
    let window_id = mapped.window.clone();

    niri.layout.set_fullscreen(&window_id, true);
    f.double_roundtrip(id);

    let window = f.client(id).window(&surface);
    assert_snapshot!(
        window.format_recent_configures(),
        @"size: 1920 × 1080, bounds: 1920 × 1080, states: [Activated, Fullscreen]"
    );
    let window = f.client(id).window(&surface);
    window.ack_last_and_commit();
    f.roundtrip(id);

    let _ = f.client(id).window(&surface).recent_configures();

    // Client requests fullscreen again, e.g. video fullscreen inside an already-fullscreen window.
    f.client(id).window(&surface).set_fullscreen(None);
    f.double_roundtrip(id);

    // This still needs to produce a fullscreen configure for the client path.
    let window = f.client(id).window(&surface);
    assert_snapshot!(
        window.format_recent_configures(),
        @"size: 1920 × 1080, bounds: 1920 × 1080, states: [Activated, Fullscreen]"
    );
}

#[test]
fn unfullscreen_before_fullscreen_ack_doesnt_prevent_view_offset_save_restore() {
    let (mut f, id, _surface) = set_up();

    let window2 = f.client(id).create_window();
    let surface2 = window2.surface.clone();
    window2.commit();
    f.roundtrip(id);

    let window2 = f.client(id).window(&surface2);
    window2.attach_new_buffer();
    window2.set_size(200, 200);
    window2.ack_last_and_commit();
    f.double_roundtrip(id);

    let _ = f.client(id).window(&surface2).recent_configures();

    let niri = f.niri();
    let mapped2 = niri.layout.windows().last().unwrap().1;
    let window2_id = mapped2.window.clone();

    // The view position is at the first window.
    assert_snapshot!(niri.layout.active_workspace().unwrap().tiling_space().view_pos(), @"0");

    // Fullscreen window2 and send the configure so we can clear pending.
    niri.layout.set_fullscreen(&window2_id, true);
    f.double_roundtrip(id);

    // Before acking, unfullscreen the column, clearing the pending fullscreen flag.
    f.niri().layout.set_fullscreen(&window2_id, false);

    // If any fullscreen configure arrives, handle it; otherwise this path is now a no-op.
    let fullscreen_configures = {
        let window2 = f.client(id).window(&surface2);
        window2.format_recent_configures()
    };
    assert_snapshot!(fullscreen_configures, @"");
    if !fullscreen_configures.is_empty() {
        let window2 = f.client(id).window(&surface2);
        let (_, configure) = window2.configures_received.last().unwrap();
        window2.set_size(configure.size.0 as u16, configure.size.1 as u16);
        window2.ack_last_and_commit();
        f.double_roundtrip(id);
        f.niri_complete_animations();
    }

    // The view position is now at the fullscreen-sized window2.
    assert_snapshot!(f.niri().layout.active_workspace().unwrap().tiling_space().view_pos(), @"0");

    // Handle unfullscreen configure if it arrives.
    let unfullscreen_configures = {
        let window2 = f.client(id).window(&surface2);
        window2.format_recent_configures()
    };
    assert_snapshot!(unfullscreen_configures, @"");
    if !unfullscreen_configures.is_empty() {
        let window2 = f.client(id).window(&surface2);
        window2.set_size(200, 200);
        window2.ack_last_and_commit();
        f.roundtrip(id);
        f.niri_complete_animations();
    }

    // The view position should restore to the first window.
    assert_snapshot!(f.niri().layout.active_workspace().unwrap().tiling_space().view_pos(), @"0");
}

#[test]
fn interactive_move_unfullscreen_to_tiling_restores_size() {
    let (mut f, id, surface) = set_up();

    let _ = f.client(id).window(&surface).recent_configures();

    let niri = f.niri();
    let mapped = niri.layout.windows().next().unwrap().1;
    let window = mapped.window.clone();
    niri.layout.set_fullscreen(&window, true);
    f.double_roundtrip(id);

    // This should request a fullscreen size.
    assert_snapshot!(
        f.client(id).window(&surface).format_recent_configures(),
        @"size: 1920 × 1080, bounds: 1920 × 1080, states: [Activated, Fullscreen]"
    );

    // Start an interactive move which causes an unfullscreen.
    let output = f.niri_output(1);
    let niri = f.niri();
    let mapped = niri.layout.windows().next().unwrap().1;
    let window = mapped.window.clone();
    niri.layout
        .interactive_move_begin(window.clone(), &output, Point::default());
    niri.layout.interactive_move_update(
        &window,
        Point::from((1000., 0.)),
        output,
        Point::default(),
    );
    f.double_roundtrip(id);

    // This should request the tiled size.
    assert_snapshot!(
        f.client(id).window(&surface).format_recent_configures(),
        @"size: 1920 × 1080, bounds: 1920 × 1080, states: [Activated]"
    );
}

#[test]
fn a_maximize_request_is_answered_and_ignored() {
    // sway's `handle_request_maximize` only schedules a configure: there is no maximized state
    // for the request to set. The client must get its answer, and nothing else may move.
    let (mut f, id, surface) = set_up();
    let _ = f.client(id).window(&surface).recent_configures();

    let size_before = f
        .niri()
        .layout
        .workspaces()
        .find_map(|(_, _, ws)| ws.windows().next().map(|w| w.size()))
        .unwrap();

    f.client(id).window(&surface).set_maximized();
    f.double_roundtrip(id);

    // A configure, carrying no Maximized state and no new size.
    assert_snapshot!(
        f.client(id).window(&surface).format_recent_configures(),
        @"size: 1904 × 1064, bounds: 1904 × 1064, states: [Activated]"
    );

    f.client(id).window(&surface).unset_maximized();
    f.double_roundtrip(id);

    assert_snapshot!(
        f.client(id).window(&surface).format_recent_configures(),
        @"size: 1904 × 1064, bounds: 1904 × 1064, states: [Activated]"
    );

    let size_after = f
        .niri()
        .layout
        .workspaces()
        .find_map(|(_, _, ws)| ws.windows().next().map(|w| w.size()))
        .unwrap();
    assert_eq!(
        size_before, size_after,
        "the request must not resize anything"
    );
}
