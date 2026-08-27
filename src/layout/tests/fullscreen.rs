use approx::assert_abs_diff_eq;

use super::*;

#[test]
fn fullscreen() {
    let ops = [
        Op::AddOutput(1),
        Op::AddWindow {
            params: TestWindowParams::new(1),
        },
        Op::FullscreenWindow(1),
    ];

    check_ops(ops);
}

#[test]
fn fullscreen_disables_resize_hits() {
    let options = Options::from_config(&Config::default());
    let mut layout = Layout::with_options(Clock::with_time(Duration::ZERO), options);

    let output = make_test_output("output0");
    layout.add_output(output.clone(), None);

    layout.add_window(
        TestWindow::new(TestWindowParams::new(1)),
        AddWindowTarget::Auto,
        None,
        false,
        ActivateWindow::Yes,
    );
    layout.add_window(
        TestWindow::new(TestWindowParams::new(2)),
        AddWindowTarget::Auto,
        None,
        false,
        ActivateWindow::Yes,
    );

    let left_tile = tile_rect(&layout, 1);
    let probe = Point::from((
        left_tile.loc.x + left_tile.size.w,
        left_tile.loc.y + left_tile.size.h / 2.0,
    ));

    layout.set_fullscreen(&1, true);

    assert!(
        layout.resize_edges_under(&output, probe).is_none(),
        "resize edge should be disabled while fullscreen is active"
    );
}

#[test]
fn fullscreen_visuals_wait_for_commit() {
    let mut layout = Layout::default();
    let output = make_test_output("output0");
    layout.add_output(output.clone(), None);

    layout.add_window(
        TestWindow::new(TestWindowParams::new(1)),
        AddWindowTarget::Auto,
        None,
        false,
        ActivateWindow::Yes,
    );
    layout.add_window(
        TestWindow::new(TestWindowParams::new(2)),
        AddWindowTarget::Auto,
        None,
        false,
        ActivateWindow::Yes,
    );

    let right_tile = tile_rect(&layout, 2);
    let probe = Point::from((
        right_tile.loc.x + right_tile.size.w / 2.0,
        right_tile.loc.y + right_tile.size.h / 2.0,
    ));

    let hit = layout
        .window_under(&output, probe)
        .expect("window 2 should be visible");
    assert_eq!(*hit.0.id(), 2);

    layout.set_fullscreen(&1, true);

    let hit = layout
        .window_under(&output, probe)
        .expect("other tiles should remain visible until fullscreen commit");
    assert_eq!(*hit.0.id(), 2);

    let window = layout
        .windows()
        .find(|(_, win)| *win.id() == 1)
        .map(|(_, win)| win.clone())
        .expect("window 1 should exist");
    assert!(
        window.communicate(),
        "fullscreen configure should resize window 1"
    );
    layout.update_window(window.id(), None);

    let hit = layout
        .window_under(&output, probe)
        .expect("fullscreen window should cover the previous probe after commit");
    assert_eq!(*hit.0.id(), 1);
}

#[test]
fn unfullscreen_window_in_column() {
    let ops = [
        Op::AddOutput(1),
        Op::AddWindow {
            params: TestWindowParams::new(1),
        },
        Op::AddWindow {
            params: TestWindowParams::new(2),
        },
        Op::ConsumeOrExpelWindowLeft { id: None },
        Op::SetFullscreenWindow {
            window: 2,
            is_fullscreen: false,
        },
    ];

    check_ops(ops);
}

#[test]
fn fullscreen_then_expelling_a_new_window_keeps_the_tree_consistent() {
    let ops = [
        Op::AddOutput(1),
        Op::AddWindow {
            params: TestWindowParams::new(0),
        },
        Op::FullscreenWindow(0),
        Op::AddWindow {
            params: TestWindowParams::new(1),
        },
        Op::ConsumeOrExpelWindowRight { id: None },
    ];

    check_ops(ops);
}

#[test]
fn fullscreen_then_consuming_a_new_window_keeps_the_tree_consistent() {
    let ops = [
        Op::AddOutput(1),
        Op::AddWindow {
            params: TestWindowParams::new(0),
        },
        Op::FullscreenWindow(0),
        Op::AddWindow {
            params: TestWindowParams::new(1),
        },
        Op::ConsumeWindowIntoColumn,
    ];

    check_ops(ops);
}

#[test]
fn fullscreen_toggled_twice_in_a_row_keeps_the_tree_consistent() {
    let ops = [
        Op::AddOutput(1),
        Op::AddWindow {
            params: TestWindowParams::new(0),
        },
        Op::FullscreenWindow(0),
        Op::FullscreenWindow(0),
    ];

    check_ops(ops);
}

#[test]
fn fullscreen_of_an_inactive_tile_in_a_column_keeps_the_tree_consistent() {
    let ops = [
        Op::AddOutput(1),
        Op::AddWindow {
            params: TestWindowParams::new(0),
        },
        Op::AddWindow {
            params: TestWindowParams::new(1),
        },
        Op::ConsumeOrExpelWindowLeft { id: None },
        Op::FullscreenWindow(0),
    ];

    check_ops(ops);
}

#[test]
fn one_window_in_column_becomes_weight_1_after_fullscreen() {
    let ops = [
        Op::AddOutput(1),
        Op::AddWindow {
            params: TestWindowParams::new(0),
        },
        Op::AddWindow {
            params: TestWindowParams::new(1),
        },
        Op::ConsumeOrExpelWindowLeft { id: None },
        Op::AddWindow {
            params: TestWindowParams::new(2),
        },
        Op::ConsumeOrExpelWindowLeft { id: None },
        Op::SetWindowHeight {
            id: None,
            change: SizeChange::SetFixed(100),
        },
        Op::Communicate(2),
        Op::FocusWindowUp,
        Op::SetWindowHeight {
            id: None,
            change: SizeChange::SetFixed(200),
        },
        Op::Communicate(1),
        Op::CloseWindow(0),
        Op::FullscreenWindow(1),
    ];

    check_ops(ops);
}

#[test]
fn disable_tabbed_mode_in_fullscreen() {
    let ops = [
        Op::AddOutput(0),
        Op::AddWindow {
            params: TestWindowParams::new(0),
        },
        Op::AddWindow {
            params: TestWindowParams::new(1),
        },
        Op::ConsumeOrExpelWindowLeft { id: None },
        Op::ToggleColumnTabbedDisplay,
        Op::FullscreenWindow(0),
        Op::ToggleColumnTabbedDisplay,
    ];

    check_ops(ops);
}

#[test]
fn unfullscreen_with_large_border() {
    let ops = [
        Op::AddWindow {
            params: TestWindowParams::new(0),
        },
        Op::FullscreenWindow(0),
        Op::Communicate(0),
        Op::FullscreenWindow(0),
    ];

    let options = Options {
        layout: tiri_config::Layout {
            border: tiri_config::Border {
                off: false,
                width: 10000.,
                ..Default::default()
            },
            ..Default::default()
        },
        ..Default::default()
    };
    check_ops_with_options(options, ops);
}

#[test]
fn fullscreen_to_windowed_fullscreen() {
    let ops = [
        Op::AddOutput(0),
        Op::AddWindow {
            params: TestWindowParams::new(0),
        },
        Op::FullscreenWindow(0),
        Op::Communicate(0), // Make sure it goes into fullscreen.
        Op::ToggleWindowedFullscreen(0),
    ];

    check_ops(ops);
}

#[test]
fn windowed_fullscreen_to_fullscreen() {
    let ops = [
        Op::AddOutput(0),
        Op::AddWindow {
            params: TestWindowParams::new(0),
        },
        Op::FullscreenWindow(0),
        Op::Communicate(0),              // Commit fullscreen state.
        Op::ToggleWindowedFullscreen(0), // Switch is_fullscreen() to false.
        Op::FullscreenWindow(0),         // Switch is_fullscreen() back to true.
    ];

    check_ops(ops);
}

#[test]
fn move_pending_unfullscreen_window_out_of_active_column() {
    let ops = [
        Op::AddOutput(1),
        Op::AddWindow {
            params: TestWindowParams::new(1),
        },
        Op::FullscreenWindow(1),
        Op::Communicate(1),
        Op::AddWindow {
            params: TestWindowParams::new(2),
        },
        Op::ConsumeWindowIntoColumn,
        // Window 1 is now pending unfullscreen.
        Op::MoveWindowToWorkspaceDown(true),
    ];

    check_ops(ops);
}

#[test]
fn move_unfocused_pending_unfullscreen_window_out_of_active_column() {
    let ops = [
        Op::AddOutput(1),
        Op::AddWindow {
            params: TestWindowParams::new(1),
        },
        Op::FullscreenWindow(1),
        Op::Communicate(1),
        Op::AddWindow {
            params: TestWindowParams::new(2),
        },
        Op::ConsumeWindowIntoColumn,
        // Window 1 is now pending unfullscreen.
        Op::FocusWindowDown,
        Op::MoveWindowToWorkspace {
            window_id: Some(1),
            workspace_idx: 1,
            focus: true,
        },
    ];

    check_ops(ops);
}

#[test]
fn interactive_resize_on_pending_unfullscreen_column() {
    let ops = [
        Op::AddWindow {
            params: TestWindowParams::new(2),
        },
        Op::FullscreenWindow(2),
        Op::Communicate(2),
        Op::SetFullscreenWindow {
            window: 2,
            is_fullscreen: false,
        },
        Op::InteractiveResizeBegin {
            window: 2,
            edges: ResizeEdge::RIGHT,
        },
        Op::Communicate(2),
    ];

    check_ops(ops);
}

#[test]
fn interactive_move_unfullscreen_to_floating_stops_dnd_scroll() {
    let ops = [
        Op::AddOutput(3),
        Op::AddWindow {
            params: TestWindowParams {
                is_floating: true,
                ..TestWindowParams::new(4)
            },
        },
        // This moves the window to tiling.
        Op::SetFullscreenWindow {
            window: 4,
            is_fullscreen: true,
        },
        // This starts a DnD scroll since we're dragging a tiled window.
        Op::InteractiveMoveBegin {
            window: 4,
            output_idx: 3,
            px: 0.0,
            py: 0.0,
        },
        // This will cause the window to unfullscreen to floating, and should stop the DnD scroll
        // since we're no longer dragging a tiled window, but rather a floating one.
        Op::InteractiveMoveUpdate {
            window: 4,
            dx: 0.0,
            dy: 15035.31210741684,
            output_idx: 3,
            px: 0.0,
            py: 0.0,
        },
        Op::InteractiveMoveEnd { window: 4 },
    ];

    check_ops(ops);
}

#[test]
fn interactive_move_of_a_fullscreen_window_restores_it_to_floating() {
    let ops = [
        Op::AddOutput(1),
        Op::AddWindow {
            params: TestWindowParams::new(1),
        },
        Op::AddWindow {
            params: TestWindowParams::new(2),
        },
        // Toggle window 1 to floating.
        Op::FocusWindow(1),
        Op::ToggleWindowFloating { id: None },
        // Fullscreen window 1 - it moves to tiling with restore_to_floating = true.
        Op::FullscreenWindow(1),
        Op::Communicate(1),
        Op::CompleteAnimations,
    ];

    let mut layout = check_ops(ops);

    // Verify window 1 is fullscreen in floating.
    let workspace = layout.active_workspace().unwrap();
    assert!(
        workspace.floating().is_fullscreen(&1),
        "window 1 should be fullscreen in floating"
    );

    let ops = [
        // Start interactive move on window 1.
        Op::InteractiveMoveBegin {
            window: 1,
            output_idx: 1,
            px: 100.,
            py: 100.,
        },
        // Update with a large delta to trigger the unmaximize.
        Op::InteractiveMoveUpdate {
            window: 1,
            dx: 1000.,
            dy: 1000.,
            output_idx: 1,
            px: 0.,
            py: 0.,
        },
    ];
    check_ops_on_layout(&mut layout, ops);

    // Window 1 should now be removed from the workspace (in the interactive move state).
    // Window 2 should be the only window in the tiling space.
    let tiling = layout.active_workspace().unwrap().container_tree();
    assert_eq!(tiling.tiles().count(), 1);
    assert!(tiling.tiles().next().unwrap().window().id() == &2);

    // In tiri, this path does not currently trigger a follow-up tiling animation.
    assert!(!layout.active_workspace().unwrap().are_animations_ongoing());
}

#[test]
fn fullscreen_during_a_dnd_gesture_keeps_the_tree_consistent() {
    let ops = [
        Op::AddOutput(1),
        Op::AddWindow {
            params: TestWindowParams::new(3),
        },
        Op::FullscreenWindow(3),
        Op::Communicate(3),
        Op::DndUpdate {
            output_idx: 1,
            px: 0.0,
            py: 0.0,
        },
        Op::FullscreenWindow(3),
        Op::Communicate(3),
    ];

    check_ops(ops);
}

#[test]
fn unfullscreen_of_a_plain_window_keeps_the_tree_consistent() {
    let ops = [
        Op::AddOutput(1),
        Op::AddWindow {
            params: TestWindowParams::new(1),
        },
        Op::AddWindow {
            params: TestWindowParams::new(2),
        },
    ];

    let mut layout = check_ops(ops);

    let ops = [
        Op::FullscreenWindow(2),
        Op::Communicate(2),
        Op::CompleteAnimations,
    ];
    check_ops_on_layout(&mut layout, ops);

    let ops = [
        Op::FullscreenWindow(2),
        Op::Communicate(2),
        Op::CompleteAnimations,
    ];
    check_ops_on_layout(&mut layout, ops);
}

#[test]
fn unfullscreen_of_a_tabbed_window_keeps_the_tree_consistent() {
    let ops = [
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
        Op::ConsumeOrExpelWindowLeft { id: None },
        Op::SetColumnDisplay(ColumnDisplay::Tabbed),
        Op::FocusColumnLeft,
        Op::FocusColumnRight,
    ];

    let mut layout = check_ops(ops);

    let ops = [
        Op::FullscreenWindow(2),
        Op::Communicate(2),
        Op::Communicate(3),
        Op::CompleteAnimations,
    ];
    check_ops_on_layout(&mut layout, ops);

    let ops = [
        Op::FullscreenWindow(3),
        Op::Communicate(3),
        Op::CompleteAnimations,
    ];
    check_ops_on_layout(&mut layout, ops);

    let ops = [Op::Communicate(2), Op::CompleteAnimations];
    check_ops_on_layout(&mut layout, ops);
}

#[test]
fn leaving_tabbed_while_fullscreen_keeps_the_tree_consistent() {
    let ops = [
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
        Op::ConsumeOrExpelWindowLeft { id: None },
        Op::SetColumnDisplay(ColumnDisplay::Tabbed),
        Op::FocusColumnLeft,
        Op::FocusColumnRight,
    ];

    let mut layout = check_ops(ops);

    let ops = [
        Op::FullscreenWindow(2),
        Op::Communicate(2),
        Op::Communicate(3),
        Op::CompleteAnimations,
    ];
    check_ops_on_layout(&mut layout, ops);

    let ops = [
        Op::SetColumnDisplay(ColumnDisplay::Normal),
        Op::Communicate(3),
        Op::CompleteAnimations,
    ];
    check_ops_on_layout(&mut layout, ops);

    let ops = [Op::Communicate(2), Op::CompleteAnimations];
    check_ops_on_layout(&mut layout, ops);
}

#[test]
fn removing_the_only_fullscreen_tile_of_a_tabbed_column() {
    let ops = [
        Op::AddOutput(1),
        Op::AddWindow {
            params: TestWindowParams::new(1),
        },
        Op::AddWindow {
            params: TestWindowParams::new(2),
        },
        Op::ConsumeOrExpelWindowLeft { id: None },
        Op::SetColumnDisplay(ColumnDisplay::Tabbed),
        Op::CompleteAnimations,
    ];

    let mut layout = check_ops(ops);

    let ops = [
        Op::FullscreenWindow(2),
        Op::Communicate(1),
        Op::Communicate(2),
        Op::CompleteAnimations,
    ];
    check_ops_on_layout(&mut layout, ops);

    let ops = [
        Op::FullscreenWindow(2),
        // The active window responds, the other tabbed window doesn't yet.
        Op::Communicate(2),
        Op::CompleteAnimations,
    ];
    check_ops_on_layout(&mut layout, ops);

    let ops = [
        // Expel the fullscreen window from the column, changing the column to non-fullscreen.
        Op::ConsumeOrExpelWindowRight { id: Some(1) },
        Op::CompleteAnimations,
    ];
    check_ops_on_layout(&mut layout, ops);
}

#[test]
fn fullscreen_directional_focus_stays_on_active_window_like_sway() {
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
        Op::FocusWindow(3),
        Op::FullscreenWindow(3),
        Op::SplitVertical,
    ]);

    let focused_before = layout.focus().map(|win| *win.id());
    check_ops_on_layout(&mut layout, [Op::FocusColumnLeft]);
    assert_eq!(
        layout.focus().map(|win| *win.id()),
        focused_before,
        "focus_left should not escape active fullscreen subtree (sway parity)"
    );

    check_ops_on_layout(&mut layout, [Op::FocusColumnRight]);
    assert_eq!(
        layout.focus().map(|win| *win.id()),
        focused_before,
        "focus_right should not escape active fullscreen subtree (sway parity)"
    );

    let workspace = layout.active_workspace().expect("active workspace");
    let tree = workspace.container_tree().debug_tree();
    assert!(
        workspace.container_tree().focused_window_id() == Some(3),
        "focus should remain on the fullscreen window after directional focus:\n{tree}"
    );
}

#[test]
fn descendant_can_move_inside_a_fullscreen_container() {
    let layout = check_ops([
        Op::AddOutput(1),
        Op::AddWindow {
            params: TestWindowParams::new(1),
        },
        Op::SetLayoutSplitV,
        Op::ToggleFullscreenFocused,
        Op::AddWindow {
            params: TestWindowParams::new(2),
        },
        Op::SplitVertical,
        Op::AddWindow {
            params: TestWindowParams::new(3),
        },
        Op::SplitVertical,
        Op::MoveWindowUp,
    ]);

    fn parent_child_count(node: &tiri_ipc::LayoutTreeNode, id: u64) -> Option<usize> {
        node.children
            .iter()
            .any(|child| child.window_id == Some(id))
            .then_some(node.children.len())
            .or_else(|| {
                node.children
                    .iter()
                    .find_map(|child| parent_child_count(child, id))
            })
    }

    let tree = layout.layout_tree();
    assert_eq!(
        tree.root
            .as_ref()
            .and_then(|root| parent_child_count(root, 1)),
        Some(2),
        "move must squash the empty single-child wrapper below the fullscreen owner",
    );
}

#[test]
fn swapping_below_fullscreen_exchanges_pending_node_boxes() {
    fn find_window(node: &tiri_ipc::LayoutTreeNode, id: u64) -> Option<&tiri_ipc::LayoutTreeNode> {
        (node.window_id == Some(id)).then_some(node).or_else(|| {
            node.children
                .iter()
                .find_map(|child| find_window(child, id))
        })
    }

    let mut layout = check_ops([
        Op::AddOutput(1),
        Op::AddWindow {
            params: TestWindowParams::new(1),
        },
        Op::AddWindow {
            params: TestWindowParams::new(2),
        },
        Op::ToggleFullscreenFocused,
        Op::SplitHorizontal,
    ]);
    let before = layout.layout_tree();
    let target_width = before
        .root
        .as_ref()
        .and_then(|root| find_window(root, 1))
        .and_then(|node| node.rect)
        .expect("target pending box")
        .width;

    check_ops_on_layout(&mut layout, [Op::SwapWithWindow(1)]);

    let tree = layout.layout_tree();
    let hidden = tree
        .root
        .as_ref()
        .and_then(|root| find_window(root, 2))
        .expect("swapped fullscreen descendant");
    assert_eq!(hidden.rect.unwrap().width, target_width);
}

#[test]
fn swapping_fullscreen_owner_focuses_its_replacement() {
    let mut layout = check_ops([
        Op::AddOutput(1),
        Op::AddWindow {
            params: TestWindowParams::new(1),
        },
        Op::AddWindow {
            params: TestWindowParams::new(2),
        },
        Op::ToggleFullscreenFocused,
    ]);

    check_ops_on_layout(&mut layout, [Op::SwapWithWindow(1)]);

    let workspace = layout.active_workspace().expect("active workspace");
    assert_eq!(workspace.container_tree().focused_window_id(), Some(1));
    assert_eq!(
        workspace
            .container_tree()
            .arena()
            .fullscreen_representative_window_id(),
        Some(&1),
    );
}

#[test]
fn swapping_fullscreen_owner_to_floating_preserves_the_floating_pending_box() {
    let mut layout = check_ops([
        Op::AddOutput(1),
        Op::AddWindow {
            params: TestWindowParams::new(1),
        },
        Op::ToggleWindowFloating { id: None },
        Op::AddWindow {
            params: TestWindowParams::new(2),
        },
        Op::ToggleFullscreenFocused,
    ]);

    check_ops_on_layout(&mut layout, [Op::SwapWithWindow(1)]);

    let floating = layout
        .layout_tree()
        .floating
        .into_iter()
        .find(|node| node.window_id == Some(2))
        .expect("the old fullscreen owner should occupy the floating slot");
    let rect = floating.rect.expect("floating pending box");
    assert!(
        rect.width > 0.0 && rect.height > 0.0,
        "a hidden floating arrival must retain the slot box instead of falling back to zero"
    );
}

#[test]
fn focus_parent_cannot_select_a_container_obstructed_by_fullscreen() {
    let mut layout = check_ops([
        Op::AddOutput(1),
        Op::AddWindow {
            params: TestWindowParams::new(1),
        },
        Op::ToggleLayoutAll,
        Op::AddWindow {
            params: TestWindowParams::new(2),
        },
        Op::ToggleFullscreenFocused,
        Op::SplitToggle,
        Op::SwapWithWindow(1),
    ]);

    check_ops_on_layout(&mut layout, [Op::FocusParent]);

    let workspace = layout.active_workspace().expect("active workspace");
    assert_eq!(workspace.container_tree().focused_window_id(), Some(2));
    assert_eq!(workspace.debug_command_target(), "tiling_window");
}

#[test]
fn unfloat_group_preserves_its_fullscreen_descendant() {
    let mut layout = check_ops([
        Op::AddOutput(1),
        Op::AddWindow {
            params: TestWindowParams::new(1),
        },
        Op::ToggleWindowFloating { id: None },
        Op::SplitToggle,
        Op::AddWindow {
            params: TestWindowParams::new(2),
        },
    ]);
    let floating_rect = layout.layout_tree().floating[0]
        .rect
        .expect("floating group box");
    check_ops_on_layout(
        &mut layout,
        [
            Op::ToggleFullscreenFocused,
            Op::ToggleWindowFloating { id: None },
        ],
    );

    let workspace = layout.active_workspace().expect("active workspace");
    assert!(!workspace.is_floating(&1));
    assert!(!workspace.is_floating(&2));
    let fullscreen = layout
        .windows()
        .find(|(_, window)| *window.id() == 2)
        .map(|(_, window)| window.pending_sizing_mode().is_fullscreen());
    assert_eq!(fullscreen, Some(true));
    let root = layout.layout_tree().root.expect("unfloated split");
    let root_rect = root.rect.unwrap();
    assert!(root_rect.width > floating_rect.width);
}

#[test]
fn fullscreen_command_keeps_a_selected_container_as_the_authority() {
    let mut layout = check_ops([
        Op::AddOutput(1),
        Op::AddWindow {
            params: TestWindowParams::new(1),
        },
        Op::AddWindow {
            params: TestWindowParams::new(2),
        },
        Op::SplitVertical,
        Op::ToggleLayoutAll,
        Op::SplitVertical,
        Op::MoveColumnRight,
        Op::FocusParent,
        Op::SplitVertical,
        Op::SetLayoutSplitH,
        // A client/fullscreen API request still targets its leaf even while the container is
        // selected. The following command must revoke that client state and replace the leaf
        // authority with the selected container.
        Op::SetFullscreenWindow {
            window: 2,
            is_fullscreen: true,
        },
    ]);

    assert_eq!(
        layout
            .active_workspace()
            .expect("active workspace")
            .debug_command_target(),
        "tiling_container",
        "precondition: the container selection must survive the client fullscreen request",
    );
    check_ops_on_layout(&mut layout, [Op::ToggleFullscreenFocused]);

    let workspace = layout.active_workspace().expect("active workspace");
    let tree = workspace.container_tree().arena();
    let authority = tree.fullscreen_key().expect("fullscreen authority");

    assert!(
        tree.get_tile(authority).is_none(),
        "the selected container, not one representative window, must own fullscreen"
    );
    assert_eq!(tree.tiles_in_branch(authority).len(), 2);
    assert!(
        layout
            .windows()
            .all(|(_, window)| !window.pending_sizing_mode().is_fullscreen()),
        "container fullscreen must leave every descendant as a tiled Wayland client"
    );
}

#[test]
fn fullscreen_container_is_the_real_render_and_input_scope() {
    let mut layout = check_ops([
        Op::AddOutput(1),
        Op::AddWindow {
            params: TestWindowParams::new(1),
        },
        Op::AddWindow {
            params: TestWindowParams::new(2),
        },
        Op::SplitVertical,
        Op::AddWindow {
            params: TestWindowParams::new(3),
        },
        Op::FocusParent,
    ]);
    assert!(
        layout.active_command_can_fullscreen(),
        "the input command must accept a selected container"
    );
    check_ops_on_layout(&mut layout, [Op::ToggleFullscreenFocused]);

    for _ in 0..3 {
        check_ops_on_layout(
            &mut layout,
            [Op::Communicate(1), Op::Communicate(2), Op::Communicate(3)],
        );
        layout.update_render_elements(None);
    }
    check_ops_on_layout(&mut layout, [Op::CompleteAnimations]);

    let (rect2, rect3) = {
        let workspace = layout.active_workspace().expect("active workspace");
        assert!(
            workspace.render_above_top_layer(),
            "container fullscreen must cover exclusive layers like leaf fullscreen"
        );

        let mut visible = HashMap::new();
        let mut rects = HashMap::new();
        for (tile, pos, is_visible) in workspace.tiles_with_render_positions() {
            visible.insert(*tile.window().id(), is_visible);
            rects.insert(*tile.window().id(), Rectangle::new(pos, tile.tile_size()));
        }

        assert_eq!(visible.get(&1), Some(&false));
        assert_eq!(visible.get(&2), Some(&true));
        assert_eq!(visible.get(&3), Some(&true));
        (rects[&2], rects[&3])
    };

    assert_abs_diff_eq!(rect2.loc.x, 0.0, epsilon = 1e-5);
    assert_abs_diff_eq!(rect3.loc.x, 0.0, epsilon = 1e-5);
    assert_abs_diff_eq!(rect2.size.w, rect3.size.w, epsilon = 1e-5);

    let output = layout.outputs().next().expect("output").clone();
    let probe = Point::from((
        rect2.loc.x + rect2.size.w / 2.0,
        rect2.loc.y + rect2.size.h / 2.0,
    ));
    let (hit, _) = layout
        .window_under(&output, probe)
        .expect("fullscreen descendant under pointer");
    assert_eq!(*hit.id(), 2);

    assert!(
        layout
            .windows()
            .all(|(_, window)| !window.pending_sizing_mode().is_fullscreen()),
        "container fullscreen must not request client fullscreen from a descendant"
    );
}

#[test]
fn floating_fullscreen_container_uses_the_same_render_and_input_scope() {
    let mut layout = check_ops([
        Op::AddOutput(1),
        Op::AddWindow {
            params: TestWindowParams::new(1),
        },
        Op::AddWindow {
            params: TestWindowParams::new(2),
        },
        Op::SplitVertical,
        Op::AddWindow {
            params: TestWindowParams::new(3),
        },
        Op::FocusParent,
        Op::ToggleWindowFloating { id: None },
    ]);

    assert_eq!(
        layout
            .active_workspace()
            .expect("active workspace")
            .debug_command_target(),
        "floating_container",
        "test precondition: the multi-window floating container must remain selected",
    );
    assert!(
        layout.active_command_can_fullscreen(),
        "the real input action must accept a selected floating container",
    );
    check_ops_on_layout(&mut layout, [Op::ToggleFullscreenFocused]);

    for _ in 0..3 {
        check_ops_on_layout(
            &mut layout,
            [Op::Communicate(1), Op::Communicate(2), Op::Communicate(3)],
        );
        layout.update_render_elements(None);
    }
    check_ops_on_layout(&mut layout, [Op::CompleteAnimations]);

    let (rect2, rect3) = {
        let workspace = layout.active_workspace().expect("active workspace");
        assert!(
            workspace.render_above_top_layer(),
            "a floating container must cover exclusive layers just like a tiled one",
        );

        let tree = workspace.container_tree().arena();
        let authority = tree.fullscreen_key().expect("fullscreen authority");
        assert!(tree.is_in_floating_branch(authority));
        assert!(tree.get_tile(authority).is_none());
        assert_eq!(tree.tiles_in_branch(authority).len(), 2);

        let mut visible = HashMap::new();
        let mut rects = HashMap::new();
        for (tile, pos, is_visible) in workspace.tiles_with_render_positions() {
            visible.insert(*tile.window().id(), is_visible);
            rects.insert(*tile.window().id(), Rectangle::new(pos, tile.tile_size()));
        }

        assert_eq!(visible.get(&1), Some(&false));
        assert_eq!(visible.get(&2), Some(&true));
        assert_eq!(visible.get(&3), Some(&true));
        (rects[&2], rects[&3])
    };

    assert_abs_diff_eq!(rect2.loc.x, 0.0, epsilon = 1e-5);
    assert_abs_diff_eq!(rect2.loc.y, 0.0, epsilon = 1e-5);
    assert_abs_diff_eq!(rect3.loc.x, 0.0, epsilon = 1e-5);
    assert_abs_diff_eq!(rect2.size.w, rect3.size.w, epsilon = 1e-5);
    assert_abs_diff_eq!(rect2.size.h, rect3.size.h, epsilon = 1e-5);
    assert_abs_diff_eq!(rect3.loc.y, rect2.size.h, epsilon = 1e-5);

    let output = layout.outputs().next().expect("output").clone();
    let probe = Point::from((
        rect2.loc.x + rect2.size.w / 2.0,
        rect2.loc.y + rect2.size.h / 2.0,
    ));
    let (hit, _) = layout
        .window_under(&output, probe)
        .expect("fullscreen floating descendant under pointer");
    assert_eq!(*hit.id(), 2);

    assert!(
        layout
            .windows()
            .all(|(_, window)| !window.pending_sizing_mode().is_fullscreen()),
        "container fullscreen must not request client fullscreen from floating descendants",
    );

    check_ops_on_layout(&mut layout, [Op::ToggleFullscreenFocused]);
    let workspace = layout.active_workspace().expect("active workspace");
    assert!(workspace
        .container_tree()
        .arena()
        .fullscreen_key()
        .is_none());
    assert_eq!(layout.close_window_ids_for_active_selection(), vec![2, 3]);
}

#[test]
fn fullscreen_targets_the_container_created_by_floating_the_workspace() {
    let mut layout = check_ops([
        Op::AddOutput(1),
        Op::AddWindow {
            params: TestWindowParams::new(1),
        },
        Op::AddWindow {
            params: TestWindowParams::new(2),
        },
        Op::FocusParent,
        Op::ToggleWindowFloating { id: None },
    ]);

    let workspace = layout.active_workspace().expect("active workspace");
    assert!(workspace.floating().selected_is_container(None));
    assert_eq!(workspace.debug_command_target(), "floating_container");
    assert!(
        layout.active_command_can_fullscreen(),
        "the selected floating wrapper must pass the real input action's fullscreen gate",
    );

    check_ops_on_layout(&mut layout, [Op::ToggleFullscreenFocused]);

    let workspace = layout.active_workspace().expect("active workspace");
    let tree = workspace.container_tree().arena();
    let authority = tree.fullscreen_key().expect("fullscreen authority");
    assert!(tree.is_in_floating_branch(authority));
    assert!(tree.get_tile(authority).is_none());
    assert_eq!(tree.tiles_in_branch(authority).len(), 2);
    assert!(workspace.render_above_top_layer());

    check_ops_on_layout(&mut layout, [Op::ToggleFullscreenFocused]);
    assert!(layout
        .active_workspace()
        .expect("active workspace")
        .container_tree()
        .arena()
        .fullscreen_key()
        .is_none(),);
}

#[test]
fn split_after_floating_fullscreen_tree_changes_keeps_layout_cache_addressed() {
    check_ops([
        Op::AddOutput(1),
        Op::AddWindow {
            params: TestWindowParams::new(1),
        },
        Op::ToggleWindowFloating { id: None },
        Op::SplitToggle,
        Op::SplitToggle,
        Op::ToggleSplitLayout,
        Op::ToggleFullscreenFocused,
        Op::AddWindow {
            params: TestWindowParams::new(2),
        },
        Op::FocusWindowUp,
        Op::SplitVertical,
    ]);
}

#[test]
fn fullscreen_focus_parent_is_noop_like_sway() {
    let mut layout = check_ops([
        Op::AddOutput(1),
        Op::AddWindow {
            params: TestWindowParams::new(1),
        },
        Op::AddWindow {
            params: TestWindowParams::new(2),
        },
        Op::FocusWindow(2),
        Op::SplitVertical,
        Op::AddWindow {
            params: TestWindowParams::new(3),
        },
        Op::FocusWindow(3),
        Op::FullscreenWindow(3),
    ]);

    check_ops_on_layout(&mut layout, [Op::FocusParent]);

    let workspace = layout.active_workspace().expect("active workspace");
    assert_eq!(layout.focus().map(|win| *win.id()), Some(3));
    assert_eq!(workspace.debug_command_target(), "tiling_window");
    assert!(
        !workspace.is_tiling_workspace_context_active(),
        "focus_parent should not enter workspace context while fullscreen is active"
    );
    assert!(
        !workspace.container_tree().selected_is_container(),
        "focus_parent should not select a tiling container while fullscreen is active"
    );

    let tree = workspace.container_tree().debug_tree();
    assert!(
        workspace.container_tree().focused_window_id() == Some(3),
        "focus should remain on the fullscreen window after focus_parent:\n{tree}"
    );
}

#[test]
fn fullscreen_focus_parent_can_select_the_fullscreen_owner() {
    let layout = check_ops([
        Op::AddOutput(1),
        Op::AddWindow {
            params: TestWindowParams::new(1),
        },
        Op::FullscreenWindow(1),
        Op::AddWindow {
            params: TestWindowParams::new(2),
        },
        Op::SplitHorizontal,
        Op::FocusParent,
    ]);

    let workspace = layout.active_workspace().expect("active workspace");
    assert_eq!(layout.focus().map(|win| *win.id()), Some(1));
    assert_eq!(workspace.debug_command_target(), "tiling_container");
    assert!(workspace.container_tree().selected_is_container());
}
#[test]
fn fullscreen_open_window_does_not_steal_focus_like_sway() {
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
        Op::FocusWindow(3),
        Op::FullscreenWindow(3),
        Op::SplitVertical,
    ]);

    let focused_before = layout.focus().map(|win| *win.id());
    layout.add_window(
        TestWindow::new(TestWindowParams::new(4)),
        AddWindowTarget::Auto,
        None,
        false,
        ActivateWindow::Yes,
    );

    assert_eq!(
        layout.focus().map(|win| *win.id()),
        focused_before,
        "open_window should not steal focus from active fullscreen tiling window (sway parity)"
    );

    let workspace = layout.active_workspace().expect("active workspace");
    let tree = workspace.container_tree().debug_tree();
    assert!(
        workspace.container_tree().focused_window_id() == Some(3),
        "focus should remain on fullscreen window after opening a new tiling window:\n{tree}"
    );
}
#[test]
fn fullscreen_open_then_focus_right_stays_locked_like_sway() {
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
        Op::FocusWindow(3),
        Op::FullscreenWindow(3),
        Op::SplitVertical,
    ]);

    layout.add_window(
        TestWindow::new(TestWindowParams::new(4)),
        AddWindowTarget::Auto,
        None,
        false,
        ActivateWindow::Yes,
    );

    check_ops_on_layout(
        &mut layout,
        [
            Op::SetLayoutTabbed,
            Op::SetLayoutSplitV,
            Op::FocusColumnRight,
        ],
    );

    assert_eq!(
        layout.focus().map(|win| *win.id()),
        Some(3),
        "focus_right should remain locked on fullscreen tiling window after open/layout ops (sway parity)"
    );
}
#[test]
fn fullscreen_focus_down_can_move_within_fullscreen_subtree_like_sway() {
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
        Op::FocusWindow(3),
        Op::FullscreenWindow(3),
        Op::SplitVertical,
    ]);

    layout.add_window(
        TestWindow::new(TestWindowParams::new(4)),
        AddWindowTarget::Auto,
        None,
        false,
        ActivateWindow::Yes,
    );

    check_ops_on_layout(
        &mut layout,
        [
            Op::SetLayoutTabbed,
            Op::SetLayoutSplitV,
            Op::FocusColumnRight,
            Op::FocusWindowDown,
        ],
    );

    assert_eq!(
        layout.focus().map(|win| *win.id()),
        Some(4),
        "focus_down should move within fullscreen subtree after split/tabbed transitions (sway parity)"
    );

    check_ops_on_layout(&mut layout, [Op::FocusWindowDown]);
    assert_eq!(
        layout.focus().map(|win| *win.id()),
        Some(4),
        "second focus_down at bottom of fullscreen subtree should be no-op (no wrap, sway parity)"
    );

    layout.add_window(
        TestWindow::new(TestWindowParams::new(5)),
        AddWindowTarget::Auto,
        None,
        false,
        ActivateWindow::Yes,
    );
    assert_eq!(
        layout.focus().map(|win| *win.id()),
        Some(4),
        "open_window should not steal focus even when focus is on non-fullscreen leaf inside fullscreen subtree (sway parity)"
    );
}

#[test]
fn focus_next_sibling_moves_inside_fullscreen_container_but_not_through_it() {
    let mut layout = check_ops([
        Op::AddOutput(1),
        Op::AddWindow {
            params: TestWindowParams::new(1),
        },
        Op::ToggleFullscreenFocused,
        Op::AddWindow {
            params: TestWindowParams::new(2),
        },
        Op::SplitVertical,
        Op::AddWindow {
            params: TestWindowParams::new(3),
        },
    ]);

    check_ops_on_layout(
        &mut layout,
        [Op::FocusAlongParent {
            forward: true,
            descend: false,
        }],
    );
    assert_eq!(layout.focus().map(|win| *win.id()), Some(3));

    check_ops_on_layout(
        &mut layout,
        [Op::FocusAlongParent {
            forward: true,
            descend: false,
        }],
    );
    assert_eq!(
        layout.focus().map(|win| *win.id()),
        Some(3),
        "the fullscreen owner stops the ancestor walk before it can wrap or leave its subtree",
    );
}

#[test]
fn focus_next_can_enter_a_fullscreen_sibling_after_a_floating_swap() {
    let mut layout = check_ops([
        Op::AddOutput(1),
        Op::AddWindow {
            params: TestWindowParams::new(1),
        },
        Op::AddWindow {
            params: TestWindowParams::new(2),
        },
        Op::ToggleFullscreenFocused,
        Op::SplitVertical,
        Op::AddWindow {
            params: TestWindowParams::new(3),
        },
        Op::ToggleWindowFloating { id: None },
        Op::SwapWithWindow(1),
    ]);

    assert_eq!(layout.focus().map(|win| *win.id()), Some(2));
    check_ops_on_layout(
        &mut layout,
        [Op::FocusAlongParent {
            forward: true,
            descend: true,
        }],
    );
    assert_eq!(layout.focus().map(|win| *win.id()), Some(3));
}
#[test]
fn floating_fullscreen_roundtrip_restores_floating() {
    let ops = [
        Op::AddOutput(1),
        Op::AddWindow {
            params: TestWindowParams::new(1),
        },
        Op::ToggleWindowFloating { id: None },
        Op::FullscreenWindow(1),
        Op::Communicate(1),
        Op::FullscreenWindow(1),
    ];

    let layout = check_ops(ops);

    let workspace = layout.active_workspace().unwrap();
    assert!(workspace.is_floating(&1));
}
#[test]
fn floating_quick_fullscreen_roundtrip_restores_floating() {
    let ops = [
        Op::AddOutput(1),
        Op::AddWindow {
            params: TestWindowParams::new(1),
        },
        Op::ToggleWindowFloating { id: None },
        Op::FullscreenWindow(1),
        // No communicate here: quickly toggle fullscreen off.
        Op::FullscreenWindow(1),
    ];

    let layout = check_ops(ops);

    let workspace = layout.active_workspace().unwrap();
    assert!(workspace.is_floating(&1));
}
#[test]
fn floating_fullscreen_roundtrip_restores_floating_with_other_tiling_windows() {
    let mut floating_params = TestWindowParams::new(2);
    floating_params.is_floating = true;

    let ops = [
        Op::AddOutput(1),
        Op::AddWindow {
            params: TestWindowParams::new(1),
        },
        Op::AddWindow {
            params: floating_params,
        },
        Op::FullscreenWindow(2),
        Op::Communicate(2),
        Op::FullscreenWindow(2),
    ];

    let layout = check_ops(ops);

    let workspace = layout.active_workspace().unwrap();
    assert!(workspace.is_floating(&2));
    assert!(!workspace.is_floating(&1));
}
#[test]
fn floating_windowed_fullscreen_replaces_existing_floating_fullscreen() {
    let ops = [
        Op::AddOutput(1),
        Op::AddWindow {
            params: TestWindowParams {
                is_floating: true,
                ..TestWindowParams::new(5)
            },
        },
        Op::AddWindow {
            params: TestWindowParams {
                is_floating: true,
                ..TestWindowParams::new(4)
            },
        },
        Op::FullscreenWindow(5),
        Op::ToggleWindowedFullscreen(4),
    ];

    let layout = check_ops(ops);

    let workspace = layout.active_workspace().unwrap();
    assert!(workspace.is_floating(&5));
    assert!(workspace.is_floating(&4));

    let (_mon, win4) = layout
        .windows()
        .find(|(_, win)| *win.id() == 4)
        .expect("window 4 should exist");
    let (_mon, win5) = layout
        .windows()
        .find(|(_, win)| *win.id() == 5)
        .expect("window 5 should exist");

    assert!(win4.pending_sizing_mode().is_fullscreen());
    assert!(!win5.pending_sizing_mode().is_fullscreen());
}
#[test]
fn floating_set_fullscreen_roundtrip_restores_floating() {
    let ops = [
        Op::AddOutput(1),
        Op::AddWindow {
            params: TestWindowParams {
                is_floating: true,
                ..TestWindowParams::new(1)
            },
        },
        Op::SetFullscreenWindow {
            window: 1,
            is_fullscreen: true,
        },
        Op::SetFullscreenWindow {
            window: 1,
            is_fullscreen: false,
        },
    ];

    let layout = check_ops(ops);

    let workspace = layout.active_workspace().unwrap();
    assert!(workspace.is_floating(&1));

    let (_mon, win) = layout
        .windows()
        .find(|(_, win)| *win.id() == 1)
        .expect("window 1 should exist");
    assert!(
        !win.is_pending_windowed_fullscreen(),
        "windowed fullscreen should be cleared after roundtrip"
    );
}
#[test]
fn floating_fullscreen_roundtrip_restores_size_and_position() {
    let mut layout = check_ops([
        Op::AddOutput(1),
        Op::AddWindow {
            params: TestWindowParams {
                is_floating: true,
                ..TestWindowParams::new(1)
            },
        },
        Op::Communicate(1),
        Op::MoveFloatingWindow {
            id: Some(1),
            x: PositionChange::SetFixed(137.),
            y: PositionChange::SetFixed(91.),
            animate: false,
        },
        Op::SetWindowWidth {
            id: Some(1),
            change: SizeChange::SetFixed(777),
        },
        Op::SetWindowHeight {
            id: Some(1),
            change: SizeChange::SetFixed(444),
        },
        Op::Communicate(1),
        Op::CompleteAnimations,
    ]);

    let before = tile_rect(&layout, 1);

    check_ops_on_layout(
        &mut layout,
        [Op::SetFullscreenWindow {
            window: 1,
            is_fullscreen: true,
        }],
    );

    {
        let workspace = layout.active_workspace().unwrap();
        assert!(
            workspace.is_floating(&1),
            "window should remain floating while fullscreen is active"
        );
        assert!(
            workspace.floating().is_fullscreen(&1),
            "window should be marked as fullscreen in floating"
        );

        let (_mon, win) = layout
            .windows()
            .find(|(_, win)| *win.id() == 1)
            .expect("window 1 should exist");
        assert!(
            win.pending_sizing_mode().is_fullscreen(),
            "floating fullscreen should request real fullscreen state"
        );
    }

    check_ops_on_layout(
        &mut layout,
        [
            Op::Communicate(1),
            Op::SetFullscreenWindow {
                window: 1,
                is_fullscreen: false,
            },
        ],
    );

    {
        let workspace = layout.active_workspace().unwrap();
        assert!(
            workspace.is_floating(&1),
            "window should remain floating after unfullscreen"
        );
        assert!(
            !workspace.floating().is_fullscreen(&1),
            "fullscreen flag should be cleared"
        );

        let (_mon, win) = layout
            .windows()
            .find(|(_, win)| *win.id() == 1)
            .expect("window 1 should exist");
        assert!(
            win.pending_sizing_mode().is_normal(),
            "unfullscreen should clear the pending fullscreen state"
        );
    }

    check_ops_on_layout(&mut layout, [Op::Communicate(1), Op::CompleteAnimations]);

    let workspace = layout.active_workspace().unwrap();
    assert!(workspace.is_floating(&1));

    let after = tile_rect(&layout, 1);
    let close = |a: f64, b: f64| (a - b).abs() <= 1.0;

    assert!(
        close(before.loc.x, after.loc.x),
        "x mismatch: before={} after={}",
        before.loc.x,
        after.loc.x
    );
    assert!(
        close(before.loc.y, after.loc.y),
        "y mismatch: before={} after={}",
        before.loc.y,
        after.loc.y
    );
    assert!(
        close(before.size.w, after.size.w),
        "w mismatch: before={} after={}",
        before.size.w,
        after.size.w
    );
    assert!(
        close(before.size.h, after.size.h),
        "h mismatch: before={} after={}",
        before.size.h,
        after.size.h
    );
}
#[test]
fn floating_fullscreen_move_window_preserves_restored_position() {
    let mut layout = check_ops([
        Op::AddOutput(1),
        Op::AddWindow {
            params: TestWindowParams {
                is_floating: true,
                ..TestWindowParams::new(1)
            },
        },
        Op::Communicate(1),
        Op::MoveFloatingWindow {
            id: Some(1),
            x: PositionChange::SetFixed(137.),
            y: PositionChange::SetFixed(91.),
            animate: false,
        },
        Op::Communicate(1),
        Op::CompleteAnimations,
    ]);

    let before = tile_rect(&layout, 1);

    check_ops_on_layout(
        &mut layout,
        [
            Op::SetFullscreenWindow {
                window: 1,
                is_fullscreen: true,
            },
            Op::Communicate(1),
            Op::MoveFloatingWindow {
                id: Some(1),
                x: PositionChange::AdjustFixed(200.),
                y: PositionChange::AdjustFixed(150.),
                animate: false,
            },
            Op::SetFullscreenWindow {
                window: 1,
                is_fullscreen: false,
            },
            Op::Communicate(1),
            Op::CompleteAnimations,
        ],
    );

    let after = tile_rect(&layout, 1);
    let close = |a: f64, b: f64| (a - b).abs() <= 1.0;

    assert!(
        close(before.loc.x, after.loc.x),
        "fullscreen move should not change restored x position: before={} after={}",
        before.loc.x,
        after.loc.x
    );
    assert!(
        close(before.loc.y, after.loc.y),
        "fullscreen move should not change restored y position: before={} after={}",
        before.loc.y,
        after.loc.y
    );
}
#[test]
fn floating_fullscreen_center_window_preserves_restored_position() {
    let mut layout = check_ops([
        Op::AddOutput(1),
        Op::AddWindow {
            params: TestWindowParams {
                is_floating: true,
                ..TestWindowParams::new(1)
            },
        },
        Op::Communicate(1),
        Op::MoveFloatingWindow {
            id: Some(1),
            x: PositionChange::SetFixed(137.),
            y: PositionChange::SetFixed(91.),
            animate: false,
        },
        Op::Communicate(1),
        Op::CompleteAnimations,
    ]);

    let before = tile_rect(&layout, 1);

    check_ops_on_layout(
        &mut layout,
        [
            Op::SetFullscreenWindow {
                window: 1,
                is_fullscreen: true,
            },
            Op::Communicate(1),
            Op::CenterWindow { id: Some(1) },
            Op::SetFullscreenWindow {
                window: 1,
                is_fullscreen: false,
            },
            Op::Communicate(1),
            Op::CompleteAnimations,
        ],
    );

    let after = tile_rect(&layout, 1);
    let close = |a: f64, b: f64| (a - b).abs() <= 1.0;

    assert!(
        close(before.loc.x, after.loc.x),
        "fullscreen center should not change restored x position: before={} after={}",
        before.loc.x,
        after.loc.x
    );
    assert!(
        close(before.loc.y, after.loc.y),
        "fullscreen center should not change restored y position: before={} after={}",
        before.loc.y,
        after.loc.y
    );
}
#[test]
fn floating_fullscreen_roundtrip_restores_position_in_container_order() {
    let mut p1 = TestWindowParams::new(1);
    p1.is_floating = true;
    let mut p2 = TestWindowParams::new(2);
    p2.is_floating = true;
    let mut p3 = TestWindowParams::new(3);
    p3.is_floating = true;

    let mut layout = check_ops([
        Op::AddOutput(1),
        Op::AddWindow { params: p1 },
        Op::SplitHorizontal,
        Op::AddWindow { params: p2 },
        Op::AddWindow { params: p3 },
        Op::Communicate(1),
        Op::Communicate(2),
        Op::Communicate(3),
        Op::CompleteAnimations,
    ]);

    let ws = layout.active_workspace().unwrap();
    assert!(ws.is_floating(&1));
    assert!(ws.is_floating(&2));
    assert!(ws.is_floating(&3));

    let before1 = tile_rect(&layout, 1);
    let before2 = tile_rect(&layout, 2);
    let before3 = tile_rect(&layout, 3);

    let close = |a: f64, b: f64| (a - b).abs() <= 1.0;

    check_ops_on_layout(
        &mut layout,
        [
            Op::FocusWindow(2),
            Op::SetFullscreenWindow {
                window: 2,
                is_fullscreen: true,
            },
            Op::Communicate(2),
            Op::SetFullscreenWindow {
                window: 2,
                is_fullscreen: false,
            },
            Op::Communicate(2),
            Op::CompleteAnimations,
        ],
    );

    let after1 = tile_rect(&layout, 1);
    let after2 = tile_rect(&layout, 2);
    let after3 = tile_rect(&layout, 3);

    assert!(close(before1.loc.x, after1.loc.x));
    assert!(close(before2.loc.x, after2.loc.x));
    assert!(close(before3.loc.x, after3.loc.x));
}
#[test]
fn moving_pending_fullscreen_into_fullscreen_workspace_keeps_one_client() {
    let layout = check_ops([
        Op::AddOutput(1),
        Op::AddWindow {
            params: TestWindowParams::new(1),
        },
        Op::FullscreenWindow(1),
        Op::FocusWorkspaceDown,
        Op::AddWindow {
            params: TestWindowParams::new(2),
        },
        Op::FullscreenWindow(2),
        Op::FocusWorkspaceUp,
        Op::MoveWindowToWorkspaceDown(true),
    ]);

    let workspace = layout.active_workspace().expect("destination workspace");
    assert_eq!(
        workspace.fullscreen_window_ids(),
        vec![2],
        "the destination workspace keeps its existing fullscreen owner"
    );

    let (_, moved) = layout
        .windows()
        .find(|(_, window)| *window.id() == 1)
        .expect("moved window");
    let (_, existing) = layout
        .windows()
        .find(|(_, window)| *window.id() == 2)
        .expect("existing fullscreen window");
    assert!(!moved.pending_sizing_mode().is_fullscreen());
    assert!(existing.pending_sizing_mode().is_fullscreen());
}

#[test]
fn expel_pending_left_from_fullscreen_tabbed_column() {
    let ops = [
        Op::AddOutput(1),
        Op::AddWindow {
            params: TestWindowParams::new(1),
        },
        Op::FullscreenWindow(1),
        Op::Communicate(1),
        // 1 is now fullscreen.
        Op::ToggleColumnTabbedDisplay,
        Op::AddWindow {
            params: TestWindowParams::new(2),
        },
        Op::ConsumeOrExpelWindowLeft { id: Some(2) },
        // 2 is consumed into a fullscreen column, fullscreen is requested but not applied.
        //
        // Now, get it back out while keeping it focused.
        //
        // Importantly, we expel it *left*, which results in adding a new column with the exact
        // same active_column_idx.
        Op::FocusWindow(2),
        Op::ConsumeOrExpelWindowLeft { id: None },
    ];

    check_ops(ops);
}

#[test]
fn a_workspace_has_at_most_one_fullscreen_window() {
    // `container_set_fullscreen` disables `ws->fullscreen` before setting the new one, and
    // `ws->fullscreen` is a single pointer that does not care which of the workspace's two
    // lists the container is in (sway/tree/container.c:1375-1377 and :1263,
    // sway/tree/workspace.h:33). Fullscreening across the two sides therefore cannot leave
    // sway with two.
    let mut layout = check_ops([
        Op::AddOutput(1),
        Op::AddWindow {
            params: TestWindowParams::new(1),
        },
        Op::AddWindow {
            params: TestWindowParams::new(2),
        },
        Op::SetWindowFloating {
            id: Some(2),
            floating: true,
        },
        Op::FullscreenWindow(1),
        Op::Communicate(1),
        Op::FullscreenWindow(2),
        Op::Communicate(2),
    ]);
    layout.update_render_elements(None);

    let fullscreen = layout
        .active_workspace()
        .expect("active workspace")
        .fullscreen_window_ids();

    assert_eq!(
        fullscreen,
        vec![2],
        "fullscreening a floating window has to release the tiled one"
    );

    let (_, old_tiled) = layout
        .windows()
        .find(|(_, window)| *window.id() == 1)
        .expect("old tiled fullscreen window");
    assert!(
        !old_tiled.pending_sizing_mode().is_fullscreen(),
        "replacing workspace fullscreen must revoke the previous window state too"
    );
}
