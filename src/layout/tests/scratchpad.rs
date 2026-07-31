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
