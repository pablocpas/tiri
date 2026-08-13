use insta::assert_snapshot;

use super::*;

fn workspace_node_key(
    layout: &Layout<TestWindow>,
    window: usize,
) -> super::super::container::NodeKey {
    layout
        .workspaces()
        .find_map(|(_, _, workspace)| workspace.tiling().tree().window_key(&window))
        .expect("window in a workspace tree")
}

fn window_node_sizing(layout: &Layout<TestWindow>, window: usize) -> (f64, f64, f64, f64) {
    layout
        .workspaces()
        .find_map(|(_, _, workspace)| {
            let tree = workspace.tiling().tree();
            let key = tree.window_key(&window)?;
            tree.debug_node_sizing(key)
        })
        .expect("window size state in a workspace tree")
}

#[test]
fn moving_a_window_between_workspaces_keeps_its_node_identity() {
    let mut layout = check_ops([
        Op::AddOutput(1),
        Op::AddWindow {
            params: TestWindowParams::new(1),
        },
    ]);
    let key = workspace_node_key(&layout, 1);

    layout.move_to_workspace_down(true);

    assert_eq!(workspace_node_key(&layout, 1), key);
}

#[test]
fn moving_a_tiled_window_to_another_workspace_derives_a_new_share() {
    let mut layout = check_ops([
        Op::AddOutput(1),
        Op::AddWindow {
            params: TestWindowParams::new(1),
        },
        Op::AddWindow {
            params: TestWindowParams::new(2),
        },
        Op::ResizeWindowEdge {
            id: Some(1),
            amount: 160,
            direction: Direction::Right,
        },
        Op::FocusWorkspaceDown,
        Op::AddWindow {
            params: TestWindowParams::new(3),
        },
        Op::FocusWorkspaceUp,
    ]);

    let before = window_node_sizing(&layout, 1).0;
    assert!(
        (before - 0.5).abs() > 0.05,
        "the source share must be distinctive"
    );
    let key = workspace_node_key(&layout, 1);

    layout.move_to_workspace(Some(&1), 1, ActivateWindow::Yes);

    assert_eq!(workspace_node_key(&layout, 1), key);
    let after = window_node_sizing(&layout, 1).0;
    assert!(
        (after - 0.5).abs() < f64::EPSILON,
        "sway clears a tiled container's old fraction before attaching it to its new workspace"
    );
}

#[test]
fn moving_a_container_between_workspaces_keeps_all_node_identities() {
    let mut layout = check_ops([
        Op::AddOutput(1),
        // The workspace below already holds a window, so the container arriving there stays
        // a container. An empty one absorbs it instead, which is
        // `moving_a_container_to_an_empty_workspace_unwraps_it_like_sway`.
        Op::FocusWorkspaceDown,
        Op::AddWindow {
            params: TestWindowParams::new(3),
        },
        Op::FocusWorkspaceUp,
        Op::AddWindow {
            params: TestWindowParams::new(1),
        },
        Op::AddWindow {
            params: TestWindowParams::new(2),
        },
        Op::SetLayoutTabbed,
    ]);

    let (first, second, container) = {
        let workspace = layout
            .workspaces()
            .find_map(|(_, _, workspace)| workspace.has_window(&1).then_some(workspace))
            .expect("source workspace");
        let tree = workspace.tiling().tree();
        let first = tree.window_key(&1).expect("first source leaf");
        let second = tree.window_key(&2).expect("second source leaf");
        let container = tree.parent_of(first).expect("source container");
        assert_ne!(container, tree.workspace_root());
        assert_eq!(tree.parent_of(second), Some(container));
        (first, second, container)
    };

    layout.move_column_to_workspace_down(true);

    let workspace = layout
        .workspaces()
        .find_map(|(_, _, workspace)| workspace.has_window(&1).then_some(workspace))
        .expect("target workspace");
    let tree = workspace.tiling().tree();
    assert_eq!(tree.window_key(&1), Some(first));
    assert_eq!(tree.window_key(&2), Some(second));
    assert_eq!(tree.parent_of(first), Some(container));
    assert_eq!(tree.parent_of(second), Some(container));
}

/// sway's `container_move_to_workspace`: a container moved to an *empty* workspace is
/// unwrapped into it — the workspace takes its layout and its children, and the container is
/// reaped. Recorded in
/// `tiri-parity/fixtures/move-a-selected-container-to-another-workspace.parity`.
#[test]
fn moving_a_container_to_an_empty_workspace_unwraps_it_like_sway() {
    let layout = check_ops([
        Op::AddOutput(1),
        Op::AddWindow {
            params: TestWindowParams::new(1),
        },
        Op::AddWindow {
            params: TestWindowParams::new(2),
        },
        Op::FocusColumnLeft,
        Op::SplitVertical,
        Op::AddWindow {
            params: TestWindowParams::new(3),
        },
        Op::FocusParent,
        Op::MoveContainerToWorkspace(1, false),
        Op::FocusWorkspaceDown,
    ]);

    let workspace = layout.active_workspace().expect("active workspace");
    assert_eq!(
        workspace.debug_workspace_layout(),
        ContainerLayout::SplitV,
        "the empty workspace takes the arriving container's layout"
    );
    assert_snapshot!(
        workspace.tiling().debug_tree().as_str(),
        @"
    SplitV
      Window 1
      Window 3 *
    "
    );
}

fn assert_moving_window_out_of_workspace_group_moves_one(
    layout_mode: ContainerLayout,
    container_action: bool,
) {
    let mut layout = check_ops([
        Op::AddOutput(1),
        Op::AddWindow {
            params: TestWindowParams::new(1),
        },
        Op::AddWindow {
            params: TestWindowParams::new(2),
        },
    ]);
    layout.set_layout_mode(layout_mode);

    if container_action {
        layout.move_container_to_workspace_down(false);
    } else {
        layout.move_to_workspace_down(false);
    }
    layout.verify_invariants();

    let source = layout
        .workspaces()
        .find_map(|(_, _, workspace)| workspace.has_window(&1).then_some(workspace))
        .expect("source workspace");
    let target = layout
        .workspaces()
        .find_map(|(_, _, workspace)| workspace.has_window(&2).then_some(workspace))
        .expect("target workspace");
    assert_ne!(source.id(), target.id(), "only the active window must move");
    assert!(!source.has_window(&2));
    assert!(!target.has_window(&1));

    let tab_bars = source.tiling().tree().tab_bar_layouts();
    assert_eq!(
        tab_bars.len(),
        1,
        "the explicit tabbed/stacked container must remain rendered after the move"
    );
    assert_eq!(
        tab_bars[0].tabs.len(),
        1,
        "the tab bar model must be refreshed at the same mutation boundary"
    );
}

#[test]
fn moving_window_out_of_tabbed_workspace_moves_one_and_refreshes_tab_bar() {
    assert_moving_window_out_of_workspace_group_moves_one(ContainerLayout::Tabbed, false);
}

#[test]
fn moving_window_out_of_stacked_workspace_moves_one_and_refreshes_tab_bar() {
    assert_moving_window_out_of_workspace_group_moves_one(ContainerLayout::Stacked, false);
}

#[test]
fn moving_focused_container_out_of_tabbed_workspace_moves_one_and_refreshes_tab_bar() {
    assert_moving_window_out_of_workspace_group_moves_one(ContainerLayout::Tabbed, true);
}

#[test]
fn moving_focused_container_out_of_stacked_workspace_moves_one_and_refreshes_tab_bar() {
    assert_moving_window_out_of_workspace_group_moves_one(ContainerLayout::Stacked, true);
}

#[test]
fn moving_selected_tabbed_parent_to_workspace_moves_the_group() {
    let mut layout = check_ops([
        Op::AddOutput(1),
        Op::AddWindow {
            params: TestWindowParams::new(1),
        },
        Op::AddWindow {
            params: TestWindowParams::new(2),
        },
        Op::SetLayoutTabbed,
        Op::FocusParent,
    ]);

    layout.move_container_to_workspace_down(false);
    layout.verify_invariants();

    let first_workspace = layout
        .workspaces()
        .find_map(|(_, _, workspace)| workspace.has_window(&1).then_some(workspace.id()))
        .expect("first window workspace");
    let second_workspace = layout
        .workspaces()
        .find_map(|(_, _, workspace)| workspace.has_window(&2).then_some(workspace.id()))
        .expect("second window workspace");
    assert_eq!(
        first_workspace, second_workspace,
        "an explicitly selected parent remains the move-container target"
    );
}

#[test]
fn moving_a_window_between_outputs_keeps_its_node_identity() {
    let mut layout = check_ops([
        Op::AddOutput(1),
        Op::AddWindow {
            params: TestWindowParams::new(1),
        },
        Op::AddOutput(2),
    ]);
    let key = workspace_node_key(&layout, 1);
    let output = layout
        .outputs()
        .find(|output| output.name() == "output2")
        .cloned()
        .expect("second output");

    layout.move_to_output(Some(&1), &output, None, ActivateWindow::Yes);

    assert_eq!(workspace_node_key(&layout, 1), key);
    assert_eq!(
        layout
            .workspaces()
            .find(|(_, _, workspace)| workspace.has_window(&1))
            .and_then(|(_, _, workspace)| workspace.current_output())
            .map(Output::name),
        Some("output2".to_owned()),
    );
}

#[test]
fn an_interactive_move_between_outputs_keeps_node_identity_while_detached() {
    let mut layout = check_ops([
        Op::AddOutput(1),
        Op::AddWindow {
            params: TestWindowParams::new(1),
        },
    ]);
    let key = workspace_node_key(&layout, 1);

    check_ops_on_layout(
        &mut layout,
        [
            Op::InteractiveMoveBegin {
                window: 1,
                output_idx: 1,
                px: 0.,
                py: 0.,
            },
            Op::AddOutput(2),
            Op::InteractiveMoveUpdate {
                window: 1,
                dx: 1000.,
                dy: 0.,
                output_idx: 2,
                px: 0.,
                py: 0.,
            },
        ],
    );

    let InteractiveMoveState::Moving(move_) = layout
        .interactive_move
        .as_ref()
        .expect("detached interactive move")
    else {
        panic!("window should have crossed the interactive-move threshold");
    };
    assert_eq!(move_.tile.node_key(), key);

    check_ops_on_layout(&mut layout, [Op::InteractiveMoveEnd { window: 1 }]);
    assert_eq!(workspace_node_key(&layout, 1), key);
}

#[test]
fn sticky_and_scratchpad_roundtrips_keep_node_identity() {
    let mut layout = check_ops([
        Op::AddOutput(1),
        Op::AddWindow {
            params: TestWindowParams {
                is_floating: true,
                ..TestWindowParams::new(1)
            },
        },
    ]);
    let key = workspace_node_key(&layout, 1);

    layout.toggle_window_sticky(Some(&1));
    let sticky_key = layout
        .monitors()
        .find_map(|monitor| monitor.sticky_space.tree().window_key(&1))
        .expect("sticky node");
    assert_eq!(sticky_key, key);

    layout.toggle_window_sticky(Some(&1));
    assert_eq!(workspace_node_key(&layout, 1), key);

    layout.move_window_to_scratchpad(Some(&1));
    assert_eq!(
        layout.scratchpad.tiles().next().map(Tile::node_key),
        Some(key)
    );
    layout.scratchpad_show();
    assert_eq!(workspace_node_key(&layout, 1), key);
}

#[test]
fn empty_workspace_layout_commands_do_not_wrap_next_open() {
    let mut layout = check_ops([
        Op::AddOutput(1),
        Op::AddWindow {
            params: TestWindowParams::new(1),
        },
        Op::CloseWindow(1),
    ]);

    layout.set_layout_mode(ContainerLayout::Tabbed);
    layout.focus_child();
    layout.set_layout_mode(ContainerLayout::SplitH);
    layout.toggle_fullscreen(&99999);

    layout.add_window(
        TestWindow::new(TestWindowParams::new(2)),
        AddWindowTarget::Auto,
        None,
        None,
        false,
        false,
        ActivateWindow::default(),
    );

    let workspace = layout.active_workspace().expect("active workspace");
    assert_eq!(workspace.floating().tiles().count(), 0);
    assert_eq!(workspace.tiling().tiles().count(), 1);
    let tree = workspace.tiling().debug_tree();
    assert!(
        !workspace.tiling().has_containers(),
        "open_window after empty-workspace layout commands should create a leaf root:\n{tree}",
    );
}
// `layout X` on an empty workspace is recorded rather than written out here: it used to
// claim the layout waited for a *second* window, which is not what sway does. See
// fixtures/layout-tabbed-on-an-empty-workspace.parity.
#[test]
fn empty_workspace_uses_workspace_command_context_like_sway() {
    let mut layout = Layout::default();
    check_ops_on_layout(&mut layout, [Op::AddOutput(1)]);

    {
        let workspace = layout.active_workspace().expect("active workspace");
        assert_eq!(
            workspace.debug_command_context(),
            "workspace",
            "empty workspace commands should target workspace context",
        );
    }

    layout.split_horizontal();
    layout.set_layout_mode(ContainerLayout::Tabbed);

    layout.add_window(
        TestWindow::new(TestWindowParams::new(1)),
        AddWindowTarget::Auto,
        None,
        None,
        false,
        false,
        ActivateWindow::default(),
    );
    layout.add_window(
        TestWindow::new(TestWindowParams::new(2)),
        AddWindowTarget::Auto,
        None,
        None,
        false,
        false,
        ActivateWindow::default(),
    );

    let workspace = layout.active_workspace().expect("active workspace");
    let tree = workspace.tiling().debug_tree().replace(" *", "");
    assert!(
        tree.starts_with("Tabbed\n"),
        "empty-workspace commands should persist and apply once tiling appears:\n{tree}",
    );
}
#[test]
fn top_level_leaf_layout_noops_when_matching_workspace_layout_like_sway() {
    let mut layout = Layout::default();
    check_ops_on_layout(&mut layout, [Op::AddOutput(1)]);

    // Empty-workspace split commands set workspace layout state in sway.
    layout.split_horizontal();
    layout.split_vertical();

    layout.add_window(
        TestWindow::new(TestWindowParams::new(1)),
        AddWindowTarget::Auto,
        None,
        None,
        false,
        false,
        ActivateWindow::default(),
    );

    let before = layout
        .active_workspace()
        .expect("active workspace")
        .tiling()
        .debug_tree();
    assert!(
        !layout
            .active_workspace()
            .expect("active workspace")
            .tiling()
            .has_containers(),
        "precondition: first tiling window should remain a leaf root:\n{before}",
    );

    layout.set_layout_mode(ContainerLayout::SplitV);

    let after = layout
        .active_workspace()
        .expect("active workspace")
        .tiling()
        .debug_tree();
    assert_eq!(
        after, before,
        "layout_splitv on top-level leaf should no-op when workspace layout already is SplitV",
    );
}
#[test]
fn top_level_leaf_toggle_split_uses_workspace_layout_state_like_sway() {
    let mut layout = Layout::default();
    check_ops_on_layout(&mut layout, [Op::AddOutput(1)]);

    // Seed workspace split layout while empty.
    layout.set_layout_mode(ContainerLayout::SplitH);

    layout.add_window(
        TestWindow::new(TestWindowParams::new(1)),
        AddWindowTarget::Auto,
        None,
        None,
        false,
        false,
        ActivateWindow::default(),
    );

    let before = layout
        .active_workspace()
        .expect("active workspace")
        .tiling()
        .debug_tree();
    assert!(
        !layout
            .active_workspace()
            .expect("active workspace")
            .tiling()
            .has_containers(),
        "precondition: single top-level window should be a leaf root:\n{before}",
    );

    layout.toggle_split_layout();

    let workspace = layout.active_workspace().expect("active workspace");
    let after = workspace.tiling().debug_tree().replace(" *", "");
    // Measured against sway 1.11: `layout toggle split` on a lone window builds a splitv
    // container for it while the workspace keeps splith.
    assert_eq!(
        after.trim_end(),
        "SplitH\n  SplitV\n    Window 1",
        "toggle split on a top-level leaf should wrap it using the workspace split state",
    );
}
#[test]
fn workspace_toggle_split_uses_prev_split_layout_like_sway() {
    let mut layout = Layout::default();
    check_ops_on_layout(&mut layout, [Op::AddOutput(1)]);

    layout.set_layout_mode(ContainerLayout::SplitV);
    layout.set_layout_mode(ContainerLayout::Tabbed);
    layout.toggle_split_layout();

    layout.add_window(
        TestWindow::new(TestWindowParams::new(1)),
        AddWindowTarget::Auto,
        None,
        None,
        false,
        false,
        ActivateWindow::default(),
    );
    layout.add_window(
        TestWindow::new(TestWindowParams::new(2)),
        AddWindowTarget::Auto,
        None,
        None,
        false,
        false,
        ActivateWindow::default(),
    );

    let workspace = layout.active_workspace().expect("active workspace");
    let tree = workspace.tiling().debug_tree().replace(" *", "");
    assert!(
        tree.starts_with("SplitV\n"),
        "layout toggle split from tabbed workspace layout should restore previous split layout:\n{tree}",
    );
}
#[test]
fn tiling_focus_parent_on_root_split_sets_workspace_intent_like_sway() {
    let layout = check_ops([
        Op::AddOutput(1),
        Op::AddWindow {
            params: TestWindowParams::new(1),
        },
        Op::SplitVertical,
        Op::AddWindow {
            params: TestWindowParams::new(2),
        },
        Op::FocusWindow(1),
        Op::FocusParent,
        Op::SplitHorizontal,
        Op::AddWindow {
            params: TestWindowParams::new(3),
        },
    ]);

    let r1 = tile_rect(&layout, 1);
    let r2 = tile_rect(&layout, 2);
    let r3 = tile_rect(&layout, 3);

    // Match sway/i3 workspace semantics:
    // splitting at root focus-parent does not immediately reflow existing children;
    // it changes the workspace-level split target used for the next sibling insert.
    assert!((r1.loc.x - r2.loc.x).abs() <= 1.0);
    assert!((r1.loc.y - r2.loc.y).abs() > 1.0);
    assert!((r3.loc.x - r1.loc.x).abs() > 1.0);
    assert!((r3.loc.y - r1.loc.y).abs() <= 1.0);
}
#[test]
fn workspace_node_selection_and_focus_child_return_to_the_active_child() {
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
    ]);

    for _ in 0..4 {
        let workspace = layout.active_workspace().expect("active workspace");
        if workspace.debug_command_target() == "workspace" {
            break;
        }
        check_ops_on_layout(&mut layout, [Op::FocusParent]);
    }

    {
        let workspace = layout.active_workspace().expect("active workspace");
        assert_eq!(workspace.debug_command_target(), "workspace");
        assert!(workspace.is_tiling_workspace_context_active());
        assert!(
            !workspace.tiling().selected_is_container(),
            "the workspace is its own node, not a hidden selected container",
        );
    }

    check_ops_on_layout(&mut layout, [Op::FocusChild]);

    let workspace = layout.active_workspace().expect("active workspace");
    assert_eq!(workspace.debug_command_target(), "tiling_container");
    assert!(
        workspace.tiling().selected_is_container(),
        "focus_child from workspace context should return to the remembered root child container",
    );
    assert_eq!(layout.close_window_ids_for_active_selection(), vec![2, 3]);
}

#[test]
fn focus_parent_selection_has_visual_geometry_for_container_and_workspace() {
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
        Op::FocusParent,
    ]);

    {
        let workspace = layout.active_workspace().expect("active workspace");
        assert_eq!(workspace.debug_command_target(), "tiling_container");
        assert!(
            workspace
                .tiling()
                .debug_selection_visual_geometry()
                .is_some(),
            "a selected container must produce the geometry for its focus-parent indicator",
        );
    }

    check_ops_on_layout(&mut layout, [Op::FocusParent]);

    let workspace = layout.active_workspace().expect("active workspace");
    assert_eq!(workspace.debug_command_target(), "workspace");
    assert!(
        workspace
            .tiling()
            .debug_selection_visual_geometry()
            .is_some(),
        "the real workspace node must also produce focus-parent indicator geometry",
    );
}

#[test]
fn a_one_entry_layout_cycle_selects_that_entry_like_sway() {
    // `get_layout_toggle_list` runs over any non-empty list. With a single entry it selects
    // it whenever the current layout is not already it, and is a no-op once it is. Only the
    // bare and `split` spellings are separate toggles.
    let cycle = vec![LayoutCycleEntry::Layout(ContainerLayout::Tabbed)];
    // The command lands on whatever holds the windows: the workspace node while they are its
    // direct children, and the wrapper `layout tabbed` builds for them afterwards.
    let owning_layout = |layout: &Layout<TestWindow>| {
        layout
            .workspaces()
            .find_map(|(_, _, workspace)| {
                let tree = workspace.tiling().tree();
                let leaf = tree.window_key(&1)?;
                tree.container_info(tree.parent_of(leaf)?)
            })
            .expect("the container holding the windows")
            .0
    };

    let mut layout = check_ops([
        Op::AddOutput(1),
        Op::AddWindow {
            params: TestWindowParams::new(1),
        },
        Op::AddWindow {
            params: TestWindowParams::new(2),
        },
    ]);
    assert_eq!(owning_layout(&layout), ContainerLayout::SplitH);

    check_ops_on_layout(
        &mut layout,
        [Op::ToggleLayoutCycle {
            cycle: cycle.clone(),
        }],
    );
    assert_eq!(owning_layout(&layout), ContainerLayout::Tabbed);

    check_ops_on_layout(&mut layout, [Op::ToggleLayoutCycle { cycle }]);
    assert_eq!(
        owning_layout(&layout),
        ContainerLayout::Tabbed,
        "the only entry is already current, so the cycle has nowhere to go",
    );
}

#[test]
fn selected_floating_container_does_not_answer_for_the_tiled_side() {
    // A floating root is parented to the workspace, so it is an ancestry descendant of the
    // workspace root. Only branch membership separates the two sides: without it a selected
    // floating container makes the tiling pass draw its own selection indicator over the
    // floating one, and suppresses the tiled focus ring.
    let mut layout = check_ops([
        Op::AddOutput(1),
        Op::AddWindow {
            params: TestWindowParams::new(1),
        },
        Op::AddWindow {
            params: TestWindowParams::new(2),
        },
        Op::FocusWindow(2),
        Op::ToggleWindowFloating { id: None },
        Op::SplitHorizontal,
        Op::AddWindow {
            params: TestWindowParams::new(3),
        },
    ]);
    check_ops_on_layout(&mut layout, [Op::FocusParent]);

    let workspace = layout.active_workspace().expect("active workspace");
    assert_eq!(workspace.debug_command_target(), "floating_container");
    assert!(
        !workspace.tiling().selected_is_container(),
        "a floating container is not a selected container of the tiled branch",
    );
    assert_eq!(
        workspace.tiling().debug_selection_visual_geometry(),
        None,
        "the tiling pass must not draw a selection indicator for a floating container",
    );
}

#[test]
fn closing_selected_container_descends_to_surviving_window() {
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
        Op::SplitVertical,
        Op::AddWindow {
            params: TestWindowParams::new(4),
        },
        Op::AddWindow {
            params: TestWindowParams::new(5),
        },
        Op::FocusParent,
    ]);

    assert_eq!(
        layout.close_window_ids_for_active_selection(),
        vec![3, 4, 5]
    );
    for id in [3, 4, 5] {
        layout.remove_window(&id, Transaction::new());
    }

    let workspace = layout.active_workspace().expect("active workspace");
    assert_eq!(workspace.debug_command_target(), "tiling_window");
    assert_eq!(layout.focus().map(|window| *window.id()), Some(2));
    assert_eq!(
        layout.close_window_ids_for_active_selection(),
        vec![2],
        "the next close must target the surviving leaf, not its workspace parent",
    );
}

#[test]
fn adding_window_while_tiling_workspace_context_drops_the_elevation() {
    // Measured against sway 1.11: `focus parent` onto the workspace, then opening a window,
    // leaves commands aimed at that window — `layout stacking` afterwards builds a container
    // rather than making the workspace stacked. Opening a window answers the question the
    // elevation was asking.
    let mut layout = check_ops([
        Op::AddOutput(1),
        Op::AddWindow {
            params: TestWindowParams::new(1),
        },
    ]);

    for _ in 0..4 {
        let workspace = layout.active_workspace().expect("active workspace");
        if workspace.debug_command_target() == "workspace" {
            break;
        }
        check_ops_on_layout(&mut layout, [Op::FocusParent]);
    }

    {
        let workspace = layout.active_workspace().expect("active workspace");
        assert_eq!(workspace.debug_command_target(), "workspace");
        assert!(workspace.is_tiling_workspace_context_active());
    }

    check_ops_on_layout(
        &mut layout,
        [Op::AddWindow {
            params: TestWindowParams::new(2),
        }],
    );

    let workspace = layout.active_workspace().expect("active workspace");
    assert!(
        !workspace.is_tiling_workspace_context_active(),
        "the new window is what commands target now, not the workspace",
    );
    assert_eq!(workspace.debug_command_target(), "tiling_window");
}

#[test]
fn primary_active_workspace_idx_not_updated_on_output_add() {
    let ops = [
        Op::AddOutput(1),
        Op::AddOutput(2),
        Op::FocusOutput(1),
        Op::AddWindow {
            params: TestWindowParams::new(0),
        },
        Op::FocusOutput(2),
        Op::AddWindow {
            params: TestWindowParams::new(1),
        },
        Op::RemoveOutput(2),
        Op::FocusWorkspace(3),
        Op::AddOutput(2),
    ];

    check_ops(ops);
}
#[test]
fn window_closed_on_previous_workspace() {
    let ops = [
        Op::AddOutput(1),
        Op::AddWindow {
            params: TestWindowParams::new(0),
        },
        Op::FocusWorkspaceDown,
        Op::CloseWindow(0),
    ];

    check_ops(ops);
}
#[test]
fn removing_output_must_keep_empty_focus_on_primary() {
    let ops = [
        Op::AddOutput(1),
        Op::AddWindow {
            params: TestWindowParams::new(0),
        },
        Op::AddOutput(2),
        Op::RemoveOutput(1),
    ];

    let layout = check_ops(ops);

    let MonitorSet::Normal { monitors, .. } = layout.monitor_set else {
        unreachable!()
    };

    // The workspace from the removed output was inserted at position 0, so the active workspace
    // must change to 1 to keep the focus on the empty workspace.
    assert_eq!(monitors[0].active_workspace_idx, 1);
}
#[test]
fn move_to_workspace_by_idx_does_not_leave_empty_workspaces() {
    let ops = [
        Op::AddOutput(1),
        Op::AddWindow {
            params: TestWindowParams::new(0),
        },
        Op::AddOutput(2),
        Op::FocusOutput(2),
        Op::AddWindow {
            params: TestWindowParams::new(1),
        },
        Op::RemoveOutput(1),
        Op::MoveWindowToWorkspace {
            window_id: Some(0),
            workspace_idx: 2,
            focus: true,
        },
    ];

    let layout = check_ops(ops);

    let MonitorSet::Normal { monitors, .. } = layout.monitor_set else {
        unreachable!()
    };

    assert!(monitors[0].workspaces[1].has_windows());
}
#[test]
fn empty_workspaces_dont_move_back_to_original_output() {
    let ops = [
        Op::AddOutput(1),
        Op::AddWindow {
            params: TestWindowParams::new(1),
        },
        Op::FocusWorkspaceDown,
        Op::AddWindow {
            params: TestWindowParams::new(2),
        },
        Op::AddOutput(2),
        Op::RemoveOutput(1),
        Op::FocusWorkspace(1),
        Op::CloseWindow(1),
        Op::AddOutput(1),
    ];

    check_ops(ops);
}
#[test]
fn named_workspaces_dont_update_original_output_on_adding_window() {
    let ops = [
        Op::AddOutput(1),
        Op::SetWorkspaceName {
            new_ws_name: 1,
            ws_name: None,
        },
        Op::AddOutput(2),
        Op::RemoveOutput(1),
        Op::FocusWorkspaceUp,
        // Adding a window updates the original output for unnamed workspaces.
        Op::AddWindow {
            params: TestWindowParams::new(1),
        },
        // Connecting the previous output should move the named workspace back since its
        // original output wasn't updated.
        Op::AddOutput(1),
    ];

    let layout = check_ops(ops);
    let (mon, _, ws) = layout
        .workspaces()
        .find(|(_, _, ws)| ws.name().is_some())
        .unwrap();
    assert!(ws.name().is_some()); // Sanity check.
    let mon = mon.unwrap();
    assert_eq!(mon.output_name(), "output1");
}
#[test]
fn workspaces_update_original_output_on_moving_to_same_output() {
    let ops = [
        Op::AddOutput(1),
        Op::SetWorkspaceName {
            new_ws_name: 1,
            ws_name: None,
        },
        Op::AddOutput(2),
        Op::RemoveOutput(1),
        Op::FocusWorkspaceUp,
        Op::MoveWorkspaceToOutput(2),
        Op::AddOutput(1),
    ];

    let layout = check_ops(ops);
    let (mon, _, ws) = layout
        .workspaces()
        .find(|(_, _, ws)| ws.name().is_some())
        .unwrap();
    assert!(ws.name().is_some()); // Sanity check.
    let mon = mon.unwrap();
    assert_eq!(mon.output_name(), "output2");
}
#[test]
fn workspaces_update_original_output_on_moving_to_same_monitor() {
    let ops = [
        Op::AddOutput(1),
        Op::SetWorkspaceName {
            new_ws_name: 1,
            ws_name: None,
        },
        Op::AddOutput(2),
        Op::RemoveOutput(1),
        Op::FocusWorkspaceUp,
        Op::MoveWorkspaceToMonitor {
            ws_name: Some(1),
            output_id: 2,
        },
        Op::AddOutput(1),
    ];

    let layout = check_ops(ops);
    let (mon, _, ws) = layout
        .workspaces()
        .find(|(_, _, ws)| ws.name().is_some())
        .unwrap();
    assert!(ws.name().is_some()); // Sanity check.
    let mon = mon.unwrap();
    assert_eq!(mon.output_name(), "output2");
}
#[test]
fn workspace_cleanup_during_switch() {
    let ops = [
        Op::AddOutput(1),
        Op::AddWindow {
            params: TestWindowParams::new(1),
        },
        Op::FocusWorkspaceDown,
        Op::CloseWindow(1),
    ];

    check_ops(ops);
}
#[test]
fn workspace_transfer_during_switch() {
    let ops = [
        Op::AddOutput(1),
        Op::AddWindow {
            params: TestWindowParams::new(1),
        },
        Op::AddOutput(2),
        Op::FocusOutput(2),
        Op::AddWindow {
            params: TestWindowParams::new(2),
        },
        Op::RemoveOutput(1),
        Op::FocusWorkspaceDown,
        Op::FocusWorkspaceDown,
        Op::AddOutput(1),
    ];

    check_ops(ops);
}
#[test]
fn workspace_transfer_during_switch_from_last() {
    let ops = [
        Op::AddOutput(1),
        Op::AddWindow {
            params: TestWindowParams::new(1),
        },
        Op::AddOutput(2),
        Op::RemoveOutput(1),
        Op::FocusWorkspaceUp,
        Op::AddOutput(1),
    ];

    check_ops(ops);
}
#[test]
fn workspace_transfer_during_switch_gets_cleaned_up() {
    let ops = [
        Op::AddOutput(1),
        Op::AddWindow {
            params: TestWindowParams::new(1),
        },
        Op::RemoveOutput(1),
        Op::AddOutput(2),
        Op::MoveColumnToWorkspaceDown(true),
        Op::MoveColumnToWorkspaceDown(true),
        Op::AddOutput(1),
    ];

    check_ops(ops);
}
#[test]
fn move_workspace_to_output() {
    let ops = [
        Op::AddOutput(1),
        Op::AddOutput(2),
        Op::FocusOutput(1),
        Op::AddWindow {
            params: TestWindowParams::new(0),
        },
        Op::MoveWorkspaceToOutput(2),
    ];

    let layout = check_ops(ops);

    let MonitorSet::Normal {
        monitors,
        active_monitor_idx,
        ..
    } = layout.monitor_set
    else {
        unreachable!()
    };

    assert_eq!(active_monitor_idx, 1);
    assert_eq!(monitors[0].workspaces.len(), 1);
    assert!(!monitors[0].workspaces[0].has_windows());
    assert_eq!(monitors[1].active_workspace_idx, 0);
    assert_eq!(monitors[1].workspaces.len(), 2);
    assert!(monitors[1].workspaces[0].has_windows());
}
#[test]
fn open_right_of_on_different_workspace() {
    let ops = [
        Op::AddOutput(1),
        Op::AddWindow {
            params: TestWindowParams::new(1),
        },
        Op::FocusWorkspaceDown,
        Op::AddWindow {
            params: TestWindowParams::new(2),
        },
        Op::AddWindowNextTo {
            params: TestWindowParams::new(3),
            next_to_id: 1,
        },
    ];

    let layout = check_ops(ops);

    let MonitorSet::Normal { monitors, .. } = layout.monitor_set else {
        unreachable!()
    };

    let mon = monitors.into_iter().next().unwrap();
    assert_eq!(
        mon.active_workspace_idx, 1,
        "the second workspace must remain active"
    );
    assert_eq!(
        mon.workspaces[0].tiling().active_column_idx(),
        1,
        "the new window must become active"
    );
}
#[test]
fn open_right_of_on_different_workspace_ewaf() {
    let ops = [
        Op::AddOutput(1),
        Op::AddWindow {
            params: TestWindowParams::new(1),
        },
        Op::FocusWorkspaceDown,
        Op::AddWindow {
            params: TestWindowParams::new(2),
        },
        Op::AddWindowNextTo {
            params: TestWindowParams::new(3),
            next_to_id: 1,
        },
    ];

    let options = Options {
        layout: tiri_config::Layout {
            empty_workspace_above_first: true,
            ..Default::default()
        },
        ..Default::default()
    };
    let layout = check_ops_with_options(options, ops);

    let MonitorSet::Normal { monitors, .. } = layout.monitor_set else {
        unreachable!()
    };

    let mon = monitors.into_iter().next().unwrap();
    assert_eq!(
        mon.active_workspace_idx, 2,
        "the second workspace must remain active"
    );
    assert_eq!(
        mon.workspaces[1].tiling().active_column_idx(),
        1,
        "the new window must become active"
    );
}
#[test]
fn removing_all_outputs_preserves_empty_named_workspaces() {
    let ops = [
        Op::AddOutput(1),
        Op::AddNamedWorkspace {
            ws_name: 1,
            output_name: None,
            layout_config: None,
        },
        Op::AddNamedWorkspace {
            ws_name: 2,
            output_name: None,
            layout_config: None,
        },
        Op::RemoveOutput(1),
    ];

    let layout = check_ops(ops);

    let MonitorSet::NoOutputs { workspaces } = layout.monitor_set else {
        unreachable!()
    };

    assert_eq!(workspaces.len(), 2);
}
#[test]
fn interactive_move_onto_empty_output() {
    let ops = [
        Op::AddOutput(1),
        Op::AddWindow {
            params: TestWindowParams::new(0),
        },
        Op::InteractiveMoveBegin {
            window: 0,
            output_idx: 1,
            px: 0.,
            py: 0.,
        },
        Op::AddOutput(2),
        Op::InteractiveMoveUpdate {
            window: 0,
            dx: 1000.,
            dy: 0.,
            output_idx: 2,
            px: 0.,
            py: 0.,
        },
        Op::InteractiveMoveEnd { window: 0 },
    ];

    check_ops(ops);
}
#[test]
fn interactive_move_onto_empty_output_ewaf() {
    let ops = [
        Op::AddOutput(1),
        Op::AddWindow {
            params: TestWindowParams::new(0),
        },
        Op::InteractiveMoveBegin {
            window: 0,
            output_idx: 1,
            px: 0.,
            py: 0.,
        },
        Op::AddOutput(2),
        Op::InteractiveMoveUpdate {
            window: 0,
            dx: 1000.,
            dy: 0.,
            output_idx: 2,
            px: 0.,
            py: 0.,
        },
        Op::InteractiveMoveEnd { window: 0 },
    ];

    let options = Options {
        layout: tiri_config::Layout {
            empty_workspace_above_first: true,
            ..Default::default()
        },
        ..Default::default()
    };
    check_ops_with_options(options, ops);
}
#[test]
fn interactive_move_onto_last_workspace() {
    let ops = [
        Op::AddOutput(1),
        Op::AddWindow {
            params: TestWindowParams::new(0),
        },
        Op::InteractiveMoveBegin {
            window: 0,
            output_idx: 1,
            px: 0.,
            py: 0.,
        },
        Op::InteractiveMoveUpdate {
            window: 0,
            dx: 1000.,
            dy: 0.,
            output_idx: 1,
            px: 0.,
            py: 0.,
        },
        Op::FocusWorkspaceDown,
        Op::AdvanceAnimations { msec_delta: 1000 },
        Op::InteractiveMoveEnd { window: 0 },
    ];

    check_ops(ops);
}
#[test]
fn interactive_move_onto_first_empty_workspace() {
    let ops = [
        Op::AddOutput(1),
        Op::AddWindow {
            params: TestWindowParams::new(1),
        },
        Op::InteractiveMoveBegin {
            window: 1,
            output_idx: 1,
            px: 0.,
            py: 0.,
        },
        Op::InteractiveMoveUpdate {
            window: 1,
            dx: 1000.,
            dy: 0.,
            output_idx: 1,
            px: 0.,
            py: 0.,
        },
        Op::FocusWorkspaceUp,
        Op::AdvanceAnimations { msec_delta: 1000 },
        Op::InteractiveMoveEnd { window: 1 },
    ];
    let options = Options {
        layout: tiri_config::Layout {
            empty_workspace_above_first: true,
            ..Default::default()
        },
        ..Default::default()
    };
    check_ops_with_options(options, ops);
}
#[test]
fn output_active_workspace_is_preserved() {
    let ops = [
        Op::AddOutput(1),
        Op::AddWindow {
            params: TestWindowParams::new(1),
        },
        Op::FocusWorkspaceDown,
        Op::AddWindow {
            params: TestWindowParams::new(2),
        },
        Op::RemoveOutput(1),
        Op::AddOutput(1),
    ];

    let layout = check_ops(ops);

    let MonitorSet::Normal { monitors, .. } = layout.monitor_set else {
        unreachable!()
    };

    assert_eq!(monitors[0].active_workspace_idx, 1);
}
#[test]
fn output_active_workspace_is_preserved_with_other_outputs() {
    let ops = [
        Op::AddOutput(1),
        Op::AddOutput(2),
        Op::AddWindow {
            params: TestWindowParams::new(1),
        },
        Op::FocusWorkspaceDown,
        Op::AddWindow {
            params: TestWindowParams::new(2),
        },
        Op::RemoveOutput(1),
        Op::AddOutput(1),
    ];

    let layout = check_ops(ops);

    let MonitorSet::Normal { monitors, .. } = layout.monitor_set else {
        unreachable!()
    };

    assert_eq!(monitors[1].active_workspace_idx, 1);
}
#[test]
fn named_workspace_to_output() {
    let ops = [
        Op::AddNamedWorkspace {
            ws_name: 1,
            output_name: None,
            layout_config: None,
        },
        Op::AddOutput(1),
        Op::MoveWorkspaceToOutput(1),
        Op::FocusWorkspaceUp,
    ];
    check_ops(ops);
}
#[test]
fn named_workspace_to_output_ewaf() {
    let ops = [
        Op::AddNamedWorkspace {
            ws_name: 1,
            output_name: Some(2),
            layout_config: None,
        },
        Op::AddOutput(1),
        Op::AddOutput(2),
    ];
    let options = Options {
        layout: tiri_config::Layout {
            empty_workspace_above_first: true,
            ..Default::default()
        },
        ..Default::default()
    };
    check_ops_with_options(options, ops);
}
#[test]
fn named_workspace_insert_on_only_empty_workspace_ewaf() {
    let ops = [
        Op::AddOutput(1),
        Op::FocusWindowOrWorkspaceDown,
        Op::AdvanceAnimations { msec_delta: 1000 },
        Op::AddNamedWorkspace {
            ws_name: 1,
            output_name: None,
            layout_config: None,
        },
    ];
    let options = Options {
        layout: tiri_config::Layout {
            empty_workspace_above_first: true,
            ..Default::default()
        },
        ..Default::default()
    };
    check_ops_with_options(options, ops);
}
#[test]
fn move_window_to_empty_workspace_above_first() {
    let ops = [
        Op::AddOutput(1),
        Op::AddWindow {
            params: TestWindowParams::new(1),
        },
        Op::MoveWorkspaceUp,
        Op::MoveWorkspaceDown,
        Op::FocusWorkspaceUp,
        Op::MoveWorkspaceDown,
    ];
    let options = Options {
        layout: tiri_config::Layout {
            empty_workspace_above_first: true,
            ..Default::default()
        },
        ..Default::default()
    };
    check_ops_with_options(options, ops);
}
#[test]
fn move_window_to_different_output() {
    let ops = [
        Op::AddWindow {
            params: TestWindowParams::new(1),
        },
        Op::AddOutput(1),
        Op::AddOutput(2),
        Op::MoveWorkspaceToOutput(2),
    ];
    let options = Options {
        layout: tiri_config::Layout {
            empty_workspace_above_first: true,
            ..Default::default()
        },
        ..Default::default()
    };
    check_ops_with_options(options, ops);
}
#[test]
fn add_and_remove_output() {
    let ops = [
        Op::AddOutput(2),
        Op::AddOutput(1),
        Op::AddWindow {
            params: TestWindowParams::new(1),
        },
        Op::RemoveOutput(2),
    ];
    let options = Options {
        layout: tiri_config::Layout {
            empty_workspace_above_first: true,
            ..Default::default()
        },
        ..Default::default()
    };
    check_ops_with_options(options, ops);
}
#[test]
fn switch_ewaf_on() {
    let ops = [
        Op::AddOutput(1),
        Op::AddWindow {
            params: TestWindowParams::new(1),
        },
    ];

    let mut layout = check_ops(ops);
    layout.update_options(Options {
        layout: tiri_config::Layout {
            empty_workspace_above_first: true,
            ..Default::default()
        },
        ..Default::default()
    });
    layout.verify_invariants();
}
#[test]
fn switch_ewaf_off() {
    let ops = [
        Op::AddOutput(1),
        Op::AddWindow {
            params: TestWindowParams::new(1),
        },
    ];

    let options = Options {
        layout: tiri_config::Layout {
            empty_workspace_above_first: true,
            ..Default::default()
        },
        ..Default::default()
    };
    let mut layout = check_ops_with_options(options, ops);
    layout.update_options(Options::default());
    layout.verify_invariants();
}
#[test]
fn interactive_move_drop_on_other_output_during_animation() {
    let ops = [
        Op::AddOutput(3),
        Op::AddWindow {
            params: TestWindowParams::new(3),
        },
        Op::InteractiveMoveBegin {
            window: 3,
            output_idx: 3,
            px: 0.0,
            py: 0.0,
        },
        Op::FocusWorkspaceDown,
        Op::AddOutput(4),
        Op::InteractiveMoveUpdate {
            window: 3,
            dx: 0.0,
            dy: 8300.68619826683,
            output_idx: 4,
            px: 0.0,
            py: 0.0,
        },
        Op::RemoveOutput(4),
        Op::InteractiveMoveEnd { window: 3 },
    ];
    check_ops(ops);
}
#[test]
fn add_window_next_to_only_interactively_moved_without_outputs() {
    let ops = [
        Op::AddWindow {
            params: TestWindowParams::new(2),
        },
        Op::AddOutput(1),
        Op::InteractiveMoveBegin {
            window: 2,
            output_idx: 1,
            px: 0.0,
            py: 0.0,
        },
        Op::InteractiveMoveUpdate {
            window: 2,
            dx: 0.0,
            dy: 3586.692842955048,
            output_idx: 1,
            px: 0.0,
            py: 0.0,
        },
        Op::RemoveOutput(1),
        // We have no outputs, and the only existing window is interactively moved, meaning there
        // are no workspaces either.
        Op::AddWindowNextTo {
            params: TestWindowParams::new(3),
            next_to_id: 2,
        },
    ];

    check_ops(ops);
}
#[test]
fn interactive_move_from_workspace_with_layout_config() {
    let ops = [
        Op::AddNamedWorkspace {
            ws_name: 1,
            output_name: Some(2),
            layout_config: Some(Box::new(tiri_config::LayoutPart {
                border: Some(tiri_config::BorderRule {
                    on: true,
                    ..Default::default()
                }),
                ..Default::default()
            })),
        },
        Op::AddOutput(1),
        Op::AddWindow {
            params: TestWindowParams::new(2),
        },
        Op::InteractiveMoveBegin {
            window: 2,
            output_idx: 1,
            px: 0.0,
            py: 0.0,
        },
        Op::InteractiveMoveUpdate {
            window: 2,
            dx: 0.0,
            dy: 3586.692842955048,
            output_idx: 1,
            px: 0.0,
            py: 0.0,
        },
        // Now remove and add the output. It will have the same workspace.
        Op::RemoveOutput(1),
        Op::AddOutput(1),
        Op::InteractiveMoveUpdate {
            window: 2,
            dx: 0.0,
            dy: 0.0,
            output_idx: 1,
            px: 0.0,
            py: 0.0,
        },
        // Now move onto a different workspace.
        Op::FocusWorkspaceDown,
        Op::CompleteAnimations,
        Op::InteractiveMoveUpdate {
            window: 2,
            dx: 0.0,
            dy: 0.0,
            output_idx: 1,
            px: 0.0,
            py: 0.0,
        },
    ];

    check_ops(ops);
}
#[test]
fn windows_on_other_workspaces_remain_activated() {
    let ops = [
        Op::AddOutput(3),
        Op::AddWindow {
            params: TestWindowParams::new(3),
        },
        Op::FocusWorkspaceDown,
        Op::Refresh { is_active: true },
    ];

    let layout = check_ops(ops);
    let (_, win) = layout.windows().next().unwrap();
    assert!(win.0.pending_activated.get());
}
#[test]
fn move_window_to_workspace_with_different_active_output() {
    let ops = [
        Op::AddOutput(0),
        Op::AddOutput(1),
        Op::AddWindow {
            params: TestWindowParams::new(0),
        },
        Op::FocusOutput(1),
        Op::MoveWindowToWorkspace {
            window_id: Some(0),
            workspace_idx: 2,
            focus: true,
        },
    ];

    check_ops(ops);
}
#[test]
fn set_first_workspace_name() {
    let ops = [
        Op::AddOutput(0),
        Op::SetWorkspaceName {
            new_ws_name: 0,
            ws_name: None,
        },
    ];

    check_ops(ops);
}
#[test]
fn set_first_workspace_name_ewaf() {
    let ops = [
        Op::AddOutput(0),
        Op::SetWorkspaceName {
            new_ws_name: 0,
            ws_name: None,
        },
    ];

    let options = Options {
        layout: tiri_config::Layout {
            empty_workspace_above_first: true,
            ..Default::default()
        },
        ..Default::default()
    };
    check_ops_with_options(options, ops);
}
#[test]
fn set_last_workspace_name() {
    let ops = [
        Op::AddOutput(0),
        Op::AddWindow {
            params: TestWindowParams::new(0),
        },
        Op::FocusWorkspaceDown,
        Op::SetWorkspaceName {
            new_ws_name: 0,
            ws_name: None,
        },
    ];

    check_ops(ops);
}
#[test]
fn initial_numeric_workspace_one_is_seeded() {
    let mut layout: Layout<TestWindow> = Layout::default();
    let output = make_test_output("eDP-1");
    layout.add_output(output.clone(), None);
    layout.verify_invariants();

    let active_workspace_id = layout.active_workspace().unwrap().id();
    let workspace_count = layout
        .monitor_for_output(&output)
        .unwrap()
        .workspace_count();
    let (target_output, idx) = layout.ensure_numeric_workspace(1).unwrap();

    assert_eq!(
        target_output.as_ref().map(|out| out.name()),
        Some(output.name())
    );
    assert_eq!(idx, 0);
    assert_eq!(
        layout.find_workspace_by_number(1).map(|(_, ws)| ws.id()),
        Some(active_workspace_id),
    );
    assert_eq!(
        layout
            .monitor_for_output(&output)
            .unwrap()
            .workspace_count(),
        workspace_count,
    );
}
#[test]
fn numeric_workspace_one_is_reused_after_switching_to_two() {
    let mut layout: Layout<TestWindow> = Layout::default();
    let output = make_test_output("eDP-1");
    layout.add_output(output.clone(), None);
    layout.add_window(
        TestWindow::new(TestWindowParams::new(1)),
        AddWindowTarget::Auto,
        None,
        None,
        false,
        false,
        ActivateWindow::default(),
    );

    let initial_workspace_id = layout.active_workspace().unwrap().id();
    let (_, ws_2_idx) = layout.ensure_numeric_workspace(2).unwrap();
    let (_, ws_2) = layout.find_workspace_by_number(2).unwrap();
    layout.focus_workspace_by_id(ws_2.id(), false);

    let (target_output, ws_1_idx) = layout.ensure_numeric_workspace(1).unwrap();
    layout.verify_invariants();

    assert_eq!(
        target_output.as_ref().map(|out| out.name()),
        Some(output.name())
    );
    assert_eq!(ws_1_idx, 0);
    assert_eq!(ws_2_idx, 1);
    assert_eq!(
        layout.find_workspace_by_number(1).map(|(_, ws)| ws.id()),
        Some(initial_workspace_id),
    );

    let mon = layout.monitor_for_output(&output).unwrap();
    let names: Vec<_> = mon
        .workspaces
        .iter()
        .filter_map(|ws| ws.name().map(String::as_str))
        .collect();
    assert_eq!(names, vec!["1", "2"]);
}
#[test]
fn initial_numeric_workspace_one_keeps_empty_above_first_invariant() {
    let mut layout: Layout<TestWindow> = Layout::with_options(
        Clock::with_time(Duration::ZERO),
        Options {
            layout: tiri_config::Layout {
                empty_workspace_above_first: true,
                ..Default::default()
            },
            ..Default::default()
        },
    );
    let output = make_test_output("eDP-1");
    layout.add_output(output.clone(), None);
    layout.verify_invariants();

    let mon = layout.monitor_for_output(&output).unwrap();
    assert_eq!(mon.workspace_count(), 3);
    assert_eq!(mon.active_workspace_idx(), 1);
    assert_eq!(mon.workspaces[0].name(), None);
    assert_eq!(mon.workspaces[1].name().map(String::as_str), Some("1"));
    assert_eq!(mon.workspaces[2].name(), None);
}
fn add_window_to_numeric_workspace(
    layout: &mut Layout<TestWindow>,
    number: u32,
    window_id: usize,
) -> WorkspaceId {
    layout.ensure_numeric_workspace(number).unwrap();
    let workspace_id = layout.find_workspace_by_number(number).unwrap().1.id();
    layout.focus_workspace_by_id(workspace_id, false);
    layout.add_window(
        TestWindow::new(TestWindowParams::new(window_id)),
        AddWindowTarget::Auto,
        None,
        None,
        false,
        false,
        ActivateWindow::default(),
    );
    workspace_id
}
#[test]
fn ensure_workspace_by_name_creates_named_workspace() {
    let mut layout: Layout<TestWindow> = Layout::default();
    let output = make_test_output("eDP-1");
    layout.add_output(output.clone(), None);

    let (target_output, idx) = layout.ensure_workspace_by_name("3").unwrap();
    assert_eq!(
        target_output.as_ref().map(|out| out.name()),
        Some(output.name())
    );
    assert_eq!(idx, 1);

    let (found_idx, ws) = layout.find_workspace_by_name("3").unwrap();
    assert_eq!(found_idx, 1);
    assert_eq!(ws.name().map(String::as_str), Some("3"));
    assert!(layout.find_workspace_by_number(3).is_none());
}
#[test]
fn numeric_config_workspace_has_numeric_identity() {
    let output = make_test_output("eDP-1");
    let mut config = Config::default();
    config.workspaces.push(WorkspaceConfig {
        name: WorkspaceName("code".to_owned()),
        number: Some(2),
        open_on_output: Some(output.name()),
        layout: None,
    });
    let mut layout: Layout<TestWindow> = Layout::new(Clock::with_time(Duration::ZERO), &config);

    layout.add_output(output.clone(), None);

    let workspace_id = layout
        .find_workspace_by_number(2)
        .map(|(_, ws)| ws.id())
        .expect("configured workspace 2 must have numeric identity");
    let (target_output, idx) = layout.ensure_numeric_workspace(2).unwrap();

    assert_eq!(
        target_output.as_ref().map(|out| out.name()),
        Some(output.name())
    );
    assert_eq!(idx, 0);
    assert_eq!(
        layout.find_workspace_by_name("code").map(|(_, ws)| ws.id()),
        Some(workspace_id),
    );
    assert!(layout.find_workspace_by_name("2").is_none());
}
#[test]
fn sway_style_numeric_config_workspace_uses_prefix_as_number() {
    let output = make_test_output("eDP-1");
    let mut config = Config::default();
    config.workspaces.push(WorkspaceConfig {
        name: WorkspaceName("5:files".to_owned()),
        number: None,
        open_on_output: Some(output.name()),
        layout: None,
    });
    let mut layout: Layout<TestWindow> = Layout::new(Clock::with_time(Duration::ZERO), &config);

    layout.add_output(output.clone(), None);

    let workspace_id = layout
        .find_workspace_by_number(5)
        .map(|(_, ws)| ws.id())
        .expect("configured workspace 5 must have numeric identity");

    assert_eq!(
        layout
            .find_workspace_by_name("5:files")
            .map(|(_, ws)| ws.id()),
        Some(workspace_id),
    );
}
#[test]
fn numeric_workspaces_are_inserted_in_number_order() {
    let mut layout: Layout<TestWindow> = Layout::default();
    layout.add_output(make_test_output("eDP-1"), None);

    layout.ensure_numeric_workspace(5);
    layout.ensure_numeric_workspace(2);
    layout.ensure_numeric_workspace(3);

    let MonitorSet::Normal { monitors, .. } = &layout.monitor_set else {
        unreachable!()
    };
    let names: Vec<_> = monitors[0]
        .workspaces
        .iter()
        .filter_map(|ws| ws.name().cloned())
        .collect();
    assert_eq!(
        names,
        vec![
            "1".to_owned(),
            "2".to_owned(),
            "3".to_owned(),
            "5".to_owned(),
        ],
    );
}
#[test]
fn named_workspaces_are_inserted_after_numeric_workspaces() {
    let mut layout: Layout<TestWindow> = Layout::default();
    layout.add_output(make_test_output("eDP-1"), None);
    add_window_to_numeric_workspace(&mut layout, 1, 1);
    add_window_to_numeric_workspace(&mut layout, 5, 5);
    add_window_to_numeric_workspace(&mut layout, 2, 2);

    layout.ensure_named_workspace(&WorkspaceConfig {
        name: WorkspaceName("web".to_owned()),
        number: None,
        open_on_output: None,
        layout: None,
    });

    let MonitorSet::Normal { monitors, .. } = &layout.monitor_set else {
        unreachable!()
    };
    let names: Vec<_> = monitors[0]
        .workspaces
        .iter()
        .filter_map(|ws| ws.name().cloned())
        .collect();
    assert_eq!(
        names,
        vec![
            "1".to_owned(),
            "2".to_owned(),
            "5".to_owned(),
            "web".to_owned(),
        ],
    );
}
#[test]
fn empty_inactive_numeric_workspace_is_destroyed_without_renumbering() {
    let mut layout: Layout<TestWindow> = Layout::default();
    let output = make_test_output("eDP-1");
    layout.add_output(output.clone(), None);

    layout.ensure_numeric_workspace(3);
    let (_, ws_3) = layout.find_workspace_by_number(3).unwrap();
    layout.focus_workspace_by_id(ws_3.id(), false);
    layout.add_window(
        TestWindow::new(TestWindowParams::new(3)),
        AddWindowTarget::Auto,
        None,
        None,
        false,
        false,
        ActivateWindow::default(),
    );

    let (_, ws_1) = layout.find_workspace_by_number(1).unwrap();
    layout.focus_workspace_by_id(ws_1.id(), false);
    let MonitorSet::Normal { monitors, .. } = &mut layout.monitor_set else {
        unreachable!()
    };
    monitors[0].workspace_switch = None;
    layout.remove_window(&3, Transaction::new());
    layout.verify_invariants();

    assert!(layout.find_workspace_by_number(3).is_none());
    let mon = layout.monitor_for_output(&output).unwrap();
    let names: Vec<_> = mon
        .workspaces
        .iter()
        .filter_map(|ws| ws.name().map(String::as_str))
        .collect();
    assert_eq!(names, vec!["1"]);
}
#[test]
fn find_workspace_by_ref_index_uses_numeric_workspace_identity() {
    let mut layout: Layout<TestWindow> = Layout::default();
    layout.add_output(make_test_output("eDP-1"), None);

    layout.ensure_numeric_workspace(3);
    let (_, ws) = layout.find_workspace_by_number(3).unwrap();
    let ws_id = ws.id();

    let resolved = layout
        .find_workspace_by_ref(WorkspaceReference::Index(3))
        .map(|ws| ws.id());
    assert_eq!(resolved, Some(ws_id));
}
#[test]
fn find_workspace_by_ref_index_without_numeric_identity_returns_none() {
    let mut layout: Layout<TestWindow> = Layout::default();
    layout.add_output(make_test_output("eDP-1"), None);

    let resolved = layout.find_workspace_by_ref(WorkspaceReference::Index(2));
    assert!(resolved.is_none());
}
#[test]
fn set_workspace_name_by_index_does_not_use_positional_fallback() {
    let mut layout: Layout<TestWindow> = Layout::default();
    layout.add_output(make_test_output("eDP-1"), None);

    layout.set_workspace_name(
        "ws-should-not-be-created".to_owned(),
        Some(WorkspaceReference::Index(2)),
    );

    assert!(layout
        .find_workspace_by_name("ws-should-not-be-created")
        .is_none());
}
#[test]
fn internal_empty_workspace_tail_is_hidden_only_when_inactive() {
    let mut layout: Layout<TestWindow> = Layout::default();
    layout.add_output(make_test_output("eDP-1"), None);
    layout.ensure_numeric_workspace(1);

    let MonitorSet::Normal { monitors, .. } = &mut layout.monitor_set else {
        unreachable!()
    };
    let mon = &mut monitors[0];

    assert!(!mon.is_internal_empty_workspace(mon.active_workspace_idx()));
    assert!(mon.is_internal_empty_workspace(1));
}
#[test]
fn transient_numeric_workspace_is_cleaned_when_empty_and_unfocused() {
    let mut layout: Layout<TestWindow> = Layout::default();
    layout.add_output(make_test_output("eDP-1"), None);
    layout
        .ensure_numeric_workspace(93)
        .expect("must create transient workspace");

    {
        let MonitorSet::Normal { monitors, .. } = &mut layout.monitor_set else {
            unreachable!()
        };
        let mon = &mut monitors[0];
        let idx = mon
            .find_named_workspace_index("93")
            .expect("workspace 93 must exist");
        mon.activate_workspace(idx);
        mon.activate_workspace(0);
        // Simulate workspace switch animation completion for cleanup.
        mon.workspace_switch = None;
        mon.clean_up_workspaces();
    }

    assert!(layout.find_workspace_by_number(93).is_none());
}
#[test]
fn move_workspace_to_output_by_workspace_id_moves_correct_workspace() {
    let mut layout: Layout<TestWindow> = Layout::default();
    let output_a = make_test_output("eDP-1");
    let output_b = make_test_output("HDMI-A-1");
    layout.add_output(output_a.clone(), None);
    layout.add_output(output_b.clone(), None);
    layout.focus_output(&output_a);

    layout.ensure_numeric_workspace(10);
    let workspace_id = layout
        .find_workspace_by_number(10)
        .map(|(_, ws)| ws.id())
        .expect("workspace 10 must exist");

    layout.move_workspace_to_output_by_workspace_id(workspace_id, &output_b);

    let (_, ws) = layout
        .find_workspace_by_number(10)
        .expect("workspace 10 must still exist");
    assert_eq!(
        ws.current_output().map(|out| out.name()),
        Some(output_b.name())
    );
}
#[test]
fn numeric_workspace_lookup_reuses_workspace_on_other_output() {
    let mut layout: Layout<TestWindow> = Layout::default();
    let output_a = make_test_output("eDP-1");
    let output_b = make_test_output("HDMI-A-1");
    layout.add_output(output_a.clone(), None);
    layout.add_output(output_b.clone(), None);
    layout.focus_output(&output_a);

    let workspace_id = add_window_to_numeric_workspace(&mut layout, 2, 2);

    layout.move_workspace_to_output_by_workspace_id(workspace_id, &output_b);
    layout.focus_output(&output_a);

    let (target_output, idx) = layout
        .ensure_numeric_workspace(2)
        .expect("workspace 2 must resolve");
    layout.focus_workspace_by_id(workspace_id, false);
    layout.verify_invariants();

    let count = layout
        .monitors()
        .flat_map(|mon| mon.workspaces.iter())
        .filter(|ws| ws.name().is_some_and(|name| name == "2"))
        .count();

    assert_eq!(
        target_output.as_ref().map(|out| out.name()),
        Some(output_b.name())
    );
    assert_eq!(idx, 0);
    assert_eq!(count, 1);
    assert_eq!(
        layout.active_output().map(|out| out.name()),
        Some(output_b.name())
    );
    assert_eq!(
        layout.active_workspace().map(|ws| ws.id()),
        Some(workspace_id)
    );
}
#[test]
fn numeric_workspaces_keep_order_when_moved_between_outputs() {
    let mut layout: Layout<TestWindow> = Layout::default();
    let output_a = make_test_output("eDP-1");
    let output_b = make_test_output("HDMI-A-1");
    layout.add_output(output_a.clone(), None);
    layout.add_output(output_b.clone(), None);
    layout.focus_output(&output_a);

    add_window_to_numeric_workspace(&mut layout, 10, 10);
    add_window_to_numeric_workspace(&mut layout, 5, 5);
    add_window_to_numeric_workspace(&mut layout, 2, 2);

    for number in [5, 2] {
        let workspace_id = layout
            .find_workspace_by_number(number)
            .map(|(_, ws)| ws.id())
            .expect("numeric workspace must exist");
        layout.move_workspace_to_output_by_workspace_id(workspace_id, &output_b);
    }

    layout.verify_invariants();

    let names: Vec<_> = layout
        .monitor_for_output(&output_b)
        .unwrap()
        .workspaces
        .iter()
        .filter_map(|ws| ws.name().cloned())
        .collect();

    assert_eq!(names, vec!["2".to_owned(), "5".to_owned()]);
}
#[test]
fn numeric_workspaces_keep_order_when_outputs_are_merged() {
    let mut layout: Layout<TestWindow> = Layout::default();
    let output_a = make_test_output("eDP-1");
    let output_b = make_test_output("HDMI-A-1");
    layout.add_output(output_a.clone(), None);
    layout.add_output(output_b.clone(), None);
    layout.focus_output(&output_b);

    add_window_to_numeric_workspace(&mut layout, 5, 5);
    add_window_to_numeric_workspace(&mut layout, 2, 2);
    layout.remove_output(&output_b);
    layout.verify_invariants();

    let names: Vec<_> = layout
        .monitor_for_output(&output_a)
        .unwrap()
        .workspaces
        .iter()
        .filter_map(|ws| ws.name().cloned())
        .collect();

    assert_eq!(names, vec!["1".to_owned(), "2".to_owned(), "5".to_owned()],);
}
#[test]
fn move_workspace_to_idx_by_workspace_id_does_not_reorder_numeric_workspaces() {
    let mut layout: Layout<TestWindow> = Layout::default();
    layout.add_output(make_test_output("eDP-1"), None);
    layout.ensure_numeric_workspace(10);
    layout.ensure_numeric_workspace(20);
    layout.ensure_numeric_workspace(30);

    let workspace_id = layout
        .find_workspace_by_number(20)
        .map(|(_, ws)| ws.id())
        .expect("workspace 20 must exist");

    layout.move_workspace_to_idx_by_workspace_id(workspace_id, 0);

    let MonitorSet::Normal { monitors, .. } = &layout.monitor_set else {
        unreachable!()
    };
    let names: Vec<_> = monitors[0]
        .workspaces
        .iter()
        .filter_map(|ws| ws.name().cloned())
        .collect();
    assert_eq!(
        names,
        vec!["20".to_owned(), "10".to_owned(), "30".to_owned()]
    );
}
#[test]
fn move_workspace_to_same_monitor_doesnt_reorder() {
    let ops = [
        Op::AddOutput(0),
        Op::SetWorkspaceName {
            new_ws_name: 0,
            ws_name: None,
        },
        Op::AddWindow {
            params: TestWindowParams::new(0),
        },
        Op::FocusWorkspaceDown,
        Op::AddWindow {
            params: TestWindowParams::new(1),
        },
        Op::AddWindow {
            params: TestWindowParams::new(2),
        },
        Op::MoveWorkspaceToMonitor {
            ws_name: Some(0),
            output_id: 0,
        },
    ];

    let layout = check_ops(ops);
    let counts: Vec<_> = layout
        .workspaces()
        .map(|(_, _, ws)| ws.windows().count())
        .collect();
    assert_eq!(counts, &[1, 2, 0]);
}
#[test]
fn move_column_to_workspace_unfocused_with_multiple_monitors() {
    let ops = [
        Op::AddOutput(1),
        Op::SetWorkspaceName {
            new_ws_name: 101,
            ws_name: None,
        },
        Op::AddWindow {
            params: TestWindowParams::new(1),
        },
        Op::FocusWorkspaceDown,
        Op::SetWorkspaceName {
            new_ws_name: 102,
            ws_name: None,
        },
        Op::AddWindow {
            params: TestWindowParams::new(2),
        },
        Op::AddOutput(2),
        Op::FocusOutput(2),
        Op::SetWorkspaceName {
            new_ws_name: 201,
            ws_name: None,
        },
        Op::AddWindow {
            params: TestWindowParams::new(3),
        },
        Op::AddWindow {
            params: TestWindowParams::new(4),
        },
        Op::MoveColumnToOutput {
            output_id: 1,
            target_ws_idx: Some(0),
            activate: false,
        },
        Op::FocusOutput(1),
    ];

    let layout = check_ops(ops);

    assert_eq!(layout.active_workspace().unwrap().name().unwrap(), "ws102");

    for (mon, win) in layout.windows() {
        let mon = mon.unwrap();
        let ws = mon
            .workspaces
            .iter()
            .find(|w| w.has_window(win.id()))
            .unwrap();

        assert_eq!(
            ws.name().unwrap(),
            match win.id() {
                1 | 4 => "ws101",
                2 => "ws102",
                3 => "ws201",
                _ => unreachable!(),
            }
        );
    }
}
#[test]
fn workspace_render_geo_at_fractional_scale() {
    let ops = [
        Op::AddScaledOutput {
            id: 1,
            scale: 1.1,
            layout_config: None,
        },
        Op::AddWindow {
            params: TestWindowParams::new(1),
        },
        Op::FocusWorkspaceDown,
        Op::CompleteAnimations,
    ];

    let layout = check_ops(ops);

    let MonitorSet::Normal { monitors, .. } = &layout.monitor_set else {
        unreachable!()
    };

    let mon = &monitors[0];
    let mut iter = mon.workspaces_with_render_geo();
    let (_ws, geo) = iter.next().unwrap();
    assert!(
        iter.next().is_none(),
        "animations are completed, only one workspace should be visible"
    );
    assert_eq!(
        geo.loc.y, 0.,
        "active workspace must be at y = 0 exactly, \
         otherwise a pointer against the screen edge at y = 0 won't hit it"
    );
}
#[test]
fn killing_workspace_selection_does_not_leave_new_windows_stuck_in_workspace_context() {
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
        Op::FocusParent,
        Op::FocusParent,
    ]);

    let selected_ids = layout.close_window_ids_for_active_selection();
    assert_eq!(selected_ids, vec![1, 2, 3]);
    for id in selected_ids {
        layout.remove_window(&id, Transaction::new());
    }

    {
        let workspace = layout.active_workspace().expect("active workspace");
        assert_eq!(workspace.windows().count(), 0);
        // An empty workspace is the only thing a command could be aimed at, so the context
        // is trivially the workspace. What matters is that it does not survive the next
        // window, which the second half of this test checks.
        assert_eq!(workspace.debug_command_target(), "workspace");
        assert!(!workspace.tiling().selected_is_container());
    }

    check_ops_on_layout(
        &mut layout,
        [
            Op::AddWindow {
                params: TestWindowParams::new(4),
            },
            Op::AddWindow {
                params: TestWindowParams::new(5),
            },
            Op::AddWindow {
                params: TestWindowParams::new(6),
            },
        ],
    );

    {
        let workspace = layout.active_workspace().expect("active workspace");
        assert_eq!(workspace.debug_command_target(), "tiling_window");
        assert!(!workspace.is_tiling_workspace_context_active());
        assert_eq!(layout.focus().map(|win| *win.id()), Some(6));
    }

    layout.focus_left();
    assert_eq!(layout.focus().map(|win| *win.id()), Some(5));

    let selected_ids = layout.close_window_ids_for_active_selection();
    assert_eq!(
        selected_ids,
        vec![5],
        "kill after reopening windows should target only the focused leaf, not the whole workspace",
    );
}
#[test]
fn layout_matching_workspace_on_top_level_leaf_keeps_workspace_root_implicit() {
    let layout = check_ops([
        Op::AddOutput(1),
        Op::AddWindow {
            params: TestWindowParams::new(1),
        },
        Op::AddWindow {
            params: TestWindowParams::new(2),
        },
        Op::SetLayoutSplitH,
    ]);

    let workspace = layout.active_workspace().expect("active workspace");
    assert!(
        workspace.tiling().debug_root_is_workspace_node(),
        "layout matching workspace layout on a top-level leaf must stay in workspace context",
    );

    let tree = workspace.tiling().debug_tree();
    assert_snapshot!(
        tree.as_str(),
        @"
    SplitH
      Window 1
      Window 2 *
    "
    );
}
#[test]
fn layout_on_top_level_leaf_builds_a_wrapper_and_leaves_the_workspace_alone() {
    // Measured against sway 1.11: a layout command issued from a window cannot change the
    // workspace's own layout. A container takes the workspace's children instead, so the
    // root stays the implicit workspace and the tabbed wrapper below it is the explicit one.
    let layout = check_ops([
        Op::AddOutput(1),
        Op::AddWindow {
            params: TestWindowParams::new(1),
        },
        Op::AddWindow {
            params: TestWindowParams::new(2),
        },
        Op::SetLayoutTabbed,
    ]);

    let workspace = layout.active_workspace().expect("active workspace");
    assert!(
        workspace.tiling().debug_root_is_workspace_node(),
        "the workspace root must stay implicit: the command was aimed at a window",
    );

    let tree = workspace.tiling().debug_tree();
    assert_snapshot!(
        tree.as_str(),
        @"
    SplitH
      Tabbed
        Window 1
        Window 2 *
    "
    );
}
#[test]
fn insert_position_empty_workspace_returns_new_column() {
    use super::super::monitor::InsertPosition;

    let options = Options::from_config(&Config::default());
    let mut layout: Layout<TestWindow> =
        Layout::with_options(Clock::with_time(Duration::ZERO), options);

    let output = make_test_output("output-test");
    layout.add_output(output.clone(), None);

    // Get the workspace without any windows
    let workspace = layout.active_workspace().expect("active workspace");

    // For an empty workspace, insert position should be NewColumn(0)
    let pos = Point::from((100.0, 100.0));
    let insert_pos = workspace.tiling_insert_position(pos);

    assert!(matches!(insert_pos, InsertPosition::NewColumn(0)));
}

/// A workspace whose last tiled window leaves still has to have a focused leaf if a floating
/// one stays behind.
///
/// `seat_get_focus_inactive` answers for any non-empty container in sway, and every descent
/// into a workspace asks it. The fallback every path took when it lost its focused node
/// walked from the workspace root, which is the tiled side alone — correct while the floating
/// side was a tree of its own, because then an empty tiling tree really did mean an empty
/// workspace. With one arena it left a workspace holding a window and answering "nothing".
///
/// Found by the ops fuzz.
#[test]
fn a_workspace_left_with_only_a_floating_window_still_has_a_focused_leaf() {
    check_ops([
        Op::AddOutput(1),
        Op::AddWindow {
            params: TestWindowParams::new(1),
        },
        Op::SetWindowFloating {
            id: Some(1),
            floating: true,
        },
        Op::AddWindow {
            params: TestWindowParams::new(2),
        },
        Op::MoveColumnToWorkspaceDown(false),
    ]);
}
