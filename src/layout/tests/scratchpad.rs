use super::*;

#[test]
fn scratchpad_show_hides_focused_window() {
    let options = Options::from_config(&Config::default());
    let mut layout = Layout::with_options(Clock::with_time(Duration::ZERO), options);

    let output = make_test_output("output-test");
    layout.add_output(output.clone(), None);

    let params1 = TestWindowParams::new(1);
    let id1 = params1.id;
    layout.add_window(
        TestWindow::new(params1),
        AddWindowTarget::Auto,
        None,
        None,
        false,
        ActivateWindow::Yes,
    );

    let params2 = TestWindowParams::new(2);
    let id2 = params2.id;
    layout.add_window(
        TestWindow::new(params2),
        AddWindowTarget::Auto,
        None,
        None,
        false,
        ActivateWindow::Yes,
    );

    layout.move_window_to_scratchpad(None);

    let workspace = layout.active_workspace().expect("active workspace");
    assert!(workspace.has_window(&id1));
    assert!(!workspace.has_window(&id2));

    layout.scratchpad_show();

    let workspace = layout.active_workspace().expect("active workspace");
    assert!(workspace.has_window(&id2));
    assert!(workspace.is_floating(&id2));
    assert_eq!(workspace.active_window().unwrap().id(), &id2);

    layout.scratchpad_show();

    let workspace = layout.active_workspace().expect("active workspace");
    assert!(!workspace.has_window(&id2));
}
#[test]
fn scratchpad_show_moves_visible_between_outputs() {
    let options = Options::from_config(&Config::default());
    let mut layout = Layout::with_options(Clock::with_time(Duration::ZERO), options);

    let output_a = make_test_output("output-a");
    let output_b = make_test_output("output-b");
    layout.add_output(output_a.clone(), None);
    layout.add_output(output_b.clone(), None);

    let params1 = TestWindowParams::new(1);
    let id1 = params1.id;
    layout.add_window(
        TestWindow::new(params1),
        AddWindowTarget::Auto,
        None,
        None,
        false,
        ActivateWindow::Yes,
    );

    layout.move_window_to_scratchpad(None);
    layout.scratchpad_show();

    let workspace = layout.active_workspace().expect("active workspace");
    assert!(workspace.has_window(&id1));
    assert!(workspace.is_floating(&id1));

    layout.focus_output(&output_b);
    layout.scratchpad_show();

    let workspace = layout.active_workspace().expect("active workspace");
    assert!(workspace.has_window(&id1));
    assert!(workspace.is_floating(&id1));
}
#[test]
fn scratchpad_multiple_windows_round_robin() {
    let options = Options::from_config(&Config::default());
    let mut layout = Layout::with_options(Clock::with_time(Duration::ZERO), options);

    let output = make_test_output("output-test");
    layout.add_output(output.clone(), None);

    // Add 3 windows
    let params1 = TestWindowParams::new(1);
    let id1 = params1.id;
    layout.add_window(
        TestWindow::new(params1),
        AddWindowTarget::Auto,
        None,
        None,
        false,
        ActivateWindow::Yes,
    );

    let params2 = TestWindowParams::new(2);
    let id2 = params2.id;
    layout.add_window(
        TestWindow::new(params2),
        AddWindowTarget::Auto,
        None,
        None,
        false,
        ActivateWindow::Yes,
    );

    let params3 = TestWindowParams::new(3);
    let id3 = params3.id;
    layout.add_window(
        TestWindow::new(params3),
        AddWindowTarget::Auto,
        None,
        None,
        false,
        ActivateWindow::Yes,
    );

    // Move all 3 windows to scratchpad
    let workspace = layout.active_workspace_mut().expect("active workspace");
    assert!(workspace.focus_window_by_id(&id1));
    layout.move_window_to_scratchpad(None);

    let workspace = layout.active_workspace_mut().expect("active workspace");
    assert!(workspace.focus_window_by_id(&id2));
    layout.move_window_to_scratchpad(None);

    let workspace = layout.active_workspace_mut().expect("active workspace");
    assert!(workspace.focus_window_by_id(&id3));
    layout.move_window_to_scratchpad(None);

    // No windows visible in workspace
    let workspace = layout.active_workspace().expect("active workspace");
    assert!(!workspace.has_window(&id1));
    assert!(!workspace.has_window(&id2));
    assert!(!workspace.has_window(&id3));

    // Show scratchpad - first window should appear (round robin order depends on implementation)
    layout.scratchpad_show();
    let workspace = layout.active_workspace().expect("active workspace");
    // At least one window should be visible
    assert!(workspace.has_window(&id1) || workspace.has_window(&id2) || workspace.has_window(&id3));
}
#[test]
fn scratchpad_from_floating_preserves_floating() {
    let options = Options::from_config(&Config::default());
    let mut layout = Layout::with_options(Clock::with_time(Duration::ZERO), options);

    let output = make_test_output("output-test");
    layout.add_output(output.clone(), None);

    // Add a window and make it floating
    let params = TestWindowParams::new(1);
    let id = params.id;
    layout.add_window(
        TestWindow::new(params),
        AddWindowTarget::Auto,
        None,
        None,
        false,
        ActivateWindow::Yes,
    );

    // Set as floating
    layout.set_window_floating(Some(&id), true);

    let workspace = layout.active_workspace().expect("active workspace");
    assert!(workspace.is_floating(&id));

    // Move to scratchpad
    layout.move_window_to_scratchpad(None);

    let workspace = layout.active_workspace().expect("active workspace");
    assert!(!workspace.has_window(&id));

    // Show from scratchpad - should appear as floating
    layout.scratchpad_show();

    let workspace = layout.active_workspace().expect("active workspace");
    assert!(workspace.has_window(&id));
    assert!(workspace.is_floating(&id));
}
#[test]
fn scratchpad_from_tiling_becomes_floating() {
    let options = Options::from_config(&Config::default());
    let mut layout = Layout::with_options(Clock::with_time(Duration::ZERO), options);

    let output = make_test_output("output-test");
    layout.add_output(output.clone(), None);

    // Add a tiling window
    let params = TestWindowParams::new(1);
    let id = params.id;
    layout.add_window(
        TestWindow::new(params),
        AddWindowTarget::Auto,
        None,
        None,
        false,
        ActivateWindow::Yes,
    );

    let workspace = layout.active_workspace().expect("active workspace");
    assert!(!workspace.is_floating(&id));

    // Move to scratchpad
    layout.move_window_to_scratchpad(None);

    // Show from scratchpad - should appear as floating (scratchpad windows are always floating)
    layout.scratchpad_show();

    let workspace = layout.active_workspace().expect("active workspace");
    assert!(workspace.has_window(&id));
    assert!(workspace.is_floating(&id));
}
#[test]
fn scratchpad_move_without_outputs_cleans_up_empty_workspace() {
    let layout = check_ops([
        Op::AddWindow {
            params: TestWindowParams::new(4),
        },
        Op::MoveWindowToScratchpad { id: Some(4) },
    ]);

    let MonitorSet::NoOutputs { workspaces } = layout.monitor_set else {
        unreachable!()
    };

    assert!(workspaces.is_empty());
}
#[test]
fn move_window_to_workspace_ignores_hidden_scratchpad_window() {
    let layout = check_ops([
        Op::AddOutput(1),
        Op::AddWindow {
            params: TestWindowParams::new(5),
        },
        Op::MoveWindowUpOrToWorkspaceUp,
        Op::FocusWorkspacePrevious,
        Op::MoveWindowToScratchpad { id: None },
        Op::MoveWindowToWorkspace {
            window_id: Some(5),
            workspace_idx: 0,
            focus: true,
        },
    ]);

    let workspace = layout.active_workspace().expect("active workspace");
    assert!(!workspace.has_window(&5));
}
#[test]
fn scratchpad_show_keeps_empty_workspace_tail() {
    let layout = check_ops([
        Op::AddWindow {
            params: TestWindowParams::new(1),
        },
        Op::AddOutput(1),
        Op::MoveWindowToScratchpad { id: None },
        Op::FocusWorkspace(1),
        Op::ScratchpadShow,
    ]);

    let MonitorSet::Normal { monitors, .. } = layout.monitor_set else {
        unreachable!()
    };

    let monitor = monitors.into_iter().next().unwrap();
    assert!(!monitor.workspaces.last().unwrap().has_windows());
}
#[test]
fn scratchpad_show_after_move_to_workspace_cleans_empty_non_active_workspace() {
    let layout = check_ops([
        Op::AddWindow {
            params: TestWindowParams::new(1),
        },
        Op::AddOutput(1),
        Op::MoveWindowToScratchpad { id: None },
        Op::ScratchpadShow,
        Op::MoveColumnToWorkspace(1, false),
        Op::ScratchpadShow,
    ]);

    let MonitorSet::Normal { monitors, .. } = layout.monitor_set else {
        unreachable!()
    };

    let monitor = monitors.into_iter().next().unwrap();
    for (idx, ws) in monitor.workspaces.iter().enumerate().skip(1) {
        if idx != monitor.active_workspace_idx && idx != monitor.workspaces.len() - 1 {
            assert!(
                ws.has_windows() || ws.name().is_some(),
                "workspace {idx} should not be left empty and unnamed"
            );
        }
    }
}
#[test]
fn move_window_to_scratchpad_during_interactive_move_doesnt_panic_on_refresh() {
    let layout = check_ops([
        Op::AddScaledOutput {
            id: 1,
            scale: 1.0,
            layout_config: None,
        },
        Op::AddWindow {
            params: TestWindowParams::new(3),
        },
        Op::InteractiveMoveBegin {
            window: 3,
            output_idx: 1,
            px: 0.0,
            py: 0.0,
        },
        Op::MoveWindowToScratchpad { id: None },
        Op::Refresh { is_active: false },
    ]);

    assert!(layout.workspaces().all(|(_, _, ws)| !ws.has_window(&3)));
}
#[test]
fn move_window_to_scratchpad_during_interactive_move_update_doesnt_panic() {
    let layout = check_ops([
        Op::AddWindow {
            params: TestWindowParams::new(2),
        },
        Op::AddOutput(4),
        Op::MoveWindowUpOrToWorkspaceUp,
        Op::InteractiveMoveBegin {
            window: 2,
            output_idx: 4,
            px: 0.0,
            py: 0.0,
        },
        Op::FocusWorkspaceUp,
        Op::MoveWindowToScratchpad { id: None },
        Op::InteractiveMoveUpdate {
            window: 2,
            dx: 0.0,
            dy: 0.0,
            output_idx: 4,
            px: 0.0,
            py: 0.0,
        },
    ]);

    assert!(layout.workspaces().all(|(_, _, ws)| !ws.has_window(&2)));
}
#[test]
fn move_to_scratchpad_cleans_empty_non_active_workspace() {
    let layout = check_ops([
        Op::AddWindow {
            params: TestWindowParams::new(2),
        },
        Op::AddOutput(1),
        Op::MoveWindowToWorkspaceDown(false),
        Op::FocusWorkspaceAutoBackAndForth(0),
        Op::MoveWindowToScratchpad { id: Some(2) },
    ]);

    let MonitorSet::Normal { monitors, .. } = layout.monitor_set else {
        unreachable!()
    };

    let monitor = monitors.into_iter().next().unwrap();
    let last_idx = monitor.workspaces.len() - 1;
    for (idx, workspace) in monitor.workspaces.iter().enumerate() {
        if idx != monitor.active_workspace_idx && idx != last_idx {
            assert!(workspace.has_windows_or_persistent_identity());
        }
    }
}
#[test]
fn sticky_toggle_requires_floating() {
    let options = Options::from_config(&Config::default());
    let mut layout = Layout::with_options(Clock::with_time(Duration::ZERO), options);

    let output = make_test_output("output-test");
    layout.add_output(output.clone(), None);

    let params = TestWindowParams::new(1);
    let id = params.id;
    layout.add_window(
        TestWindow::new(params),
        AddWindowTarget::Auto,
        None,
        None,
        false,
        ActivateWindow::Yes,
    );

    layout.toggle_window_sticky(None);

    let workspace = layout.active_workspace().expect("active workspace");
    assert!(workspace.has_window(&id));
    assert!(!window_layout(&layout, id).is_sticky);
}
#[test]
fn sticky_moves_across_workspaces_on_output() {
    let options = Options::from_config(&Config::default());
    let mut layout = Layout::with_options(Clock::with_time(Duration::ZERO), options);

    let output = make_test_output("output-test");
    layout.add_output(output.clone(), None);

    let params = TestWindowParams::new(1);
    let id = params.id;
    layout.add_window(
        TestWindow::new(params),
        AddWindowTarget::Auto,
        None,
        None,
        false,
        ActivateWindow::Yes,
    );

    layout.set_window_floating(Some(&id), true);
    layout.toggle_window_sticky(None);

    let workspace = layout.active_workspace().expect("active workspace");
    assert!(!workspace.has_window(&id));
    assert!(window_layout(&layout, id).is_sticky);

    layout.switch_workspace(1);
    let active_ws_id = layout.active_workspace().expect("active workspace").id();

    assert!(window_layout(&layout, id).is_sticky);

    // Ensure sticky window reports the active workspace id.
    let mut reported_ws = None;
    layout.with_windows(|win, _output, ws_id, _layout| {
        if *win.id() == id {
            reported_ws = ws_id;
        }
    });
    assert_eq!(reported_ws, Some(active_ws_id));

    layout.toggle_window_sticky(Some(&id));
    let workspace = layout.active_workspace().expect("active workspace");
    assert!(workspace.has_window(&id));
    assert!(!window_layout(&layout, id).is_sticky);
}
#[test]
fn scratchpad_show_hides_visible_then_shows_next() {
    let options = Options::from_config(&Config::default());
    let mut layout = Layout::with_options(Clock::with_time(Duration::ZERO), options);

    let output = make_test_output("output-test");
    layout.add_output(output.clone(), None);

    // Add 2 windows
    let params1 = TestWindowParams::new(1);
    let id1 = params1.id;
    layout.add_window(
        TestWindow::new(params1),
        AddWindowTarget::Auto,
        None,
        None,
        false,
        ActivateWindow::Yes,
    );

    let params2 = TestWindowParams::new(2);
    let id2 = params2.id;
    layout.add_window(
        TestWindow::new(params2),
        AddWindowTarget::Auto,
        None,
        None,
        false,
        ActivateWindow::Yes,
    );

    // Move both to scratchpad
    let workspace = layout.active_workspace_mut().expect("active workspace");
    assert!(workspace.focus_window_by_id(&id1));
    layout.move_window_to_scratchpad(None);

    let workspace = layout.active_workspace_mut().expect("active workspace");
    assert!(workspace.focus_window_by_id(&id2));
    layout.move_window_to_scratchpad(None);

    // Show first scratchpad window
    layout.scratchpad_show();
    let workspace = layout.active_workspace().expect("active workspace");
    let first_visible = if workspace.has_window(&id1) { id1 } else { id2 };
    assert!(workspace.has_window(&first_visible));

    // Call scratchpad_show again - should hide current and show the other
    layout.scratchpad_show();
    let workspace = layout.active_workspace().expect("active workspace");
    // First window should be hidden now
    assert!(!workspace.has_window(&first_visible));
}
#[test]
fn scratchpad_fullscreen_to_scratchpad() {
    let options = Options::from_config(&Config::default());
    let mut layout = Layout::with_options(Clock::with_time(Duration::ZERO), options);

    let output = make_test_output("output-test");
    layout.add_output(output.clone(), None);

    // Add a window
    let params = TestWindowParams::new(1);
    let id = params.id;
    layout.add_window(
        TestWindow::new(params),
        AddWindowTarget::Auto,
        None,
        None,
        false,
        ActivateWindow::Yes,
    );

    // Make fullscreen
    layout.set_fullscreen(&id, true);

    // Move to scratchpad
    layout.move_window_to_scratchpad(None);

    let workspace = layout.active_workspace().expect("active workspace");
    assert!(!workspace.has_window(&id));

    // Show from scratchpad - should appear as floating
    layout.scratchpad_show();

    let workspace = layout.active_workspace().expect("active workspace");
    assert!(workspace.has_window(&id));
    assert!(workspace.is_floating(&id));
}

/// Sending a tiled window to the scratchpad leaves the workspace with one fewer window, and
/// the survivors have to be given the space it left. Reported as "the tiling breaks, they
/// don't resize".
#[test]
fn moving_a_tiled_window_to_the_scratchpad_resizes_the_survivors() {
    let mut layout = check_ops([
        Op::AddOutput(1),
        Op::AddWindow {
            params: TestWindowParams::new(1),
        },
        Op::AddWindow {
            params: TestWindowParams::new(2),
        },
        Op::AddWindow {
            params: TestWindowParams::new(3),
        },
    ]);
    layout.update_render_elements(None);

    let before = tiled_window_rects(&layout);
    assert_eq!(before.len(), 3, "three tiled windows to start with");

    check_ops_on_layout(&mut layout, [Op::MoveWindowToScratchpad { id: Some(3) }]);
    layout.update_render_elements(None);

    let after = tiled_window_rects(&layout);
    assert_eq!(after.len(), 2, "the third window left for the scratchpad");

    // The survivors must have grown into the space the third one left: with three windows
    // each was a third of the workspace, with two each is a half.
    let widths: Vec<f64> = after.iter().map(|rect| rect.size.w).collect();
    let before_widths: Vec<f64> = before.iter().map(|rect| rect.size.w).collect();
    assert!(
        widths.iter().all(|w| *w > before_widths[0] * 1.4),
        "each survivor must be about half the workspace, not still a third: \
         before {before_widths:?}, after {widths:?}"
    );
    assert!(
        (widths[0] - widths[1]).abs() < 2.0,
        "and the two halves must match: {widths:?}"
    );
}

fn space_root(layout: &Layout<TestWindow>) -> crate::layout::container::NodeKey {
    layout
        .active_workspace()
        .expect("active workspace")
        .tiling()
        .tree()
        .workspace_root()
}

fn tiled_window_rects(
    layout: &Layout<TestWindow>,
) -> Vec<smithay::utils::Rectangle<f64, smithay::utils::Logical>> {
    layout
        .active_workspace()
        .expect("active workspace")
        .tiling()
        .tree()
        .leaf_layouts()
        .iter()
        .filter(|info| info.branch == space_root(layout))
        .map(|info| info.rect)
        .collect()
}

/// The whole scratchpad round trip: a tiled window leaves, comes back as a floating
/// scratchpad window, and goes away again. The tiled side has to be laid out correctly at
/// every step, and the scratchpad window has to get a box of its own rather than whatever
/// it had as a tile.
#[test]
fn a_scratchpad_round_trip_leaves_the_tiling_laid_out() {
    let mut layout = check_ops([
        Op::AddOutput(1),
        Op::AddWindow {
            params: TestWindowParams::new(1),
        },
        Op::AddWindow {
            params: TestWindowParams::new(2),
        },
        Op::AddWindow {
            params: TestWindowParams::new(3),
        },
    ]);
    layout.update_render_elements(None);
    let three_up = tiled_window_rects(&layout);

    check_ops_on_layout(&mut layout, [Op::MoveWindowToScratchpad { id: Some(3) }]);
    layout.update_render_elements(None);
    let hidden = tiled_window_rects(&layout);
    assert_eq!(
        hidden.len(),
        2,
        "the scratchpad window is not on the workspace"
    );
    assert!(
        hidden[0].size.w > three_up[0].size.w,
        "the survivors grew: {three_up:?} -> {hidden:?}"
    );

    check_ops_on_layout(&mut layout, [Op::ScratchpadShow]);
    layout.update_render_elements(None);
    let shown = tiled_window_rects(&layout);
    assert_eq!(
        shown.len(),
        2,
        "showing the scratchpad window does not put it back in the tiling: {shown:?}"
    );
    assert_eq!(
        shown, hidden,
        "and it does not disturb the tiled windows either"
    );

    let workspace = layout.active_workspace().expect("active workspace");
    assert!(
        workspace.is_floating(&3),
        "a shown scratchpad window floats"
    );

    check_ops_on_layout(&mut layout, [Op::ScratchpadShow]);
    layout.update_render_elements(None);
    assert_eq!(
        tiled_window_rects(&layout),
        hidden,
        "hiding it again leaves the tiling where it was"
    );
}

/// A shown scratchpad window has to be given a box. It arrives as a tile that was in the
/// tiling, so its floating group's rectangle is the one thing nothing else can supply — and
/// when it comes out as 0x0 the window is on the workspace, focused, and invisible.
#[test]
fn a_shown_scratchpad_window_has_a_size() {
    let mut layout = check_ops([
        Op::AddOutput(1),
        Op::AddWindow {
            params: TestWindowParams::new(1),
        },
        Op::AddWindow {
            params: TestWindowParams::new(2),
        },
        Op::AddWindow {
            params: TestWindowParams::new(3),
        },
        Op::MoveWindowToScratchpad { id: Some(3) },
        Op::ScratchpadShow,
    ]);
    layout.update_render_elements(None);

    let workspace = layout.active_workspace().expect("active workspace");
    let tree = workspace.tiling().tree();
    let key = tree
        .window_key(&3)
        .expect("the scratchpad window is mapped");
    let root = tree.branch_root(key);
    let area = tree
        .floating_area(root)
        .expect("a shown scratchpad window is a floating group");

    assert!(
        area.size.w > 0.0 && area.size.h > 0.0,
        "the scratchpad window's group must have a box: {area:?}"
    );
}

/// A window in the scratchpad is a window on a workspace: laid out, with a box of its own.
///
/// It used to be a detached tile in a queue, arranged by nobody. That is why showing one cost
/// a full resize handshake with a client that had been idle — the window had no box, so the
/// arrange that gave it one waited for a configure the client was in no hurry to ack, and the
/// wait was the whole transaction deadline.
#[test]
fn a_hidden_scratchpad_window_is_laid_out_while_it_is_hidden() {
    let mut layout = check_ops([
        Op::AddOutput(1),
        Op::AddWindow {
            params: TestWindowParams::new(1),
        },
        Op::AddWindow {
            params: TestWindowParams::new(2),
        },
        Op::MoveWindowToScratchpad { id: Some(2) },
    ]);
    layout.update_render_elements(None);

    let scratchpad = layout.scratchpad_for_test();
    assert!(
        scratchpad.has_window(&2),
        "the hidden window is on the scratchpad workspace"
    );

    let tree = scratchpad.tiling().tree();
    let key = tree.window_key(&2).expect("hidden window is in the arena");
    let root = tree.branch_root(key);
    let area = tree
        .floating_area(root)
        .expect("a hidden scratchpad window floats, like sway's");
    assert!(
        area.size.w > 0.0 && area.size.h > 0.0,
        "and it is laid out while hidden, so coming back is a move and not a negotiation: {area:?}"
    );
}

/// The size the scratchpad gives a window it floats.
///
/// `root_scratchpad_add_container` floats a tiled window and calls
/// `container_floating_set_default_size`, which is half the workspace's width and three
/// quarters of its height — not the size the window asked for, and not the size it had while
/// tiled. It is `floating enable` that keeps a window's own idea of its size.
///
/// sway/tree/root.c:114-119, sway/tree/container.c:959-980
#[test]
fn hiding_a_tiled_window_gives_it_half_the_width_and_three_quarters_of_the_height() {
    let mut layout = check_ops([
        Op::AddOutput(1),
        Op::AddWindow {
            params: TestWindowParams::new(1),
        },
        Op::AddWindow {
            params: TestWindowParams::new(2),
        },
        Op::MoveWindowToScratchpad { id: Some(2) },
    ]);
    layout.update_render_elements(None);

    let scratchpad = layout.scratchpad_for_test();
    let working_area = scratchpad.tiling().parent_area().size;
    let tile = scratchpad
        .tiles()
        .find(|tile| tile.window().id() == &2)
        .expect("the hidden window");
    let size = tile
        .window()
        .expected_size()
        .expect("the scratchpad asked it for a size");

    assert_eq!(
        (size.w, size.h),
        (
            (working_area.w * 0.5).floor() as i32,
            (working_area.h * 0.75).floor() as i32
        ),
        "half the workspace's width and three quarters of its height"
    );
}

/// The other half of the same rule: a window that was already floating is put away exactly as
/// it is. sway only resizes when it has to float the window itself.
#[test]
fn hiding_a_floating_window_leaves_its_size_alone() {
    let mut layout = check_ops([
        Op::AddOutput(1),
        Op::AddWindow {
            params: TestWindowParams::new(1),
        },
        Op::SetWindowFloating {
            id: Some(1),
            floating: true,
        },
    ]);
    layout.update_render_elements(None);

    let before = layout
        .active_workspace()
        .expect("active workspace")
        .tiles()
        .find(|tile| tile.window().id() == &1)
        .and_then(|tile| tile.window().expected_size())
        .expect("a floating window has a size");

    check_ops_on_layout(&mut layout, [Op::MoveWindowToScratchpad { id: Some(1) }]);
    layout.update_render_elements(None);

    let after = layout
        .scratchpad_for_test()
        .tiles()
        .find(|tile| tile.window().id() == &1)
        .and_then(|tile| tile.window().expected_size())
        .expect("the hidden window");

    assert_eq!(
        before, after,
        "an already-floating window is put away as it is"
    );
}
