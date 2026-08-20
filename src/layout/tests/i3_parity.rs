use insta::assert_snapshot;

use super::super::container::Layout as ContainerLayout;
use super::*;

#[test]
fn i3_167_workspace_layout_tabbed_groups_second_open() {
    let mut layout = Layout::default();
    check_ops_on_layout(&mut layout, [Op::AddOutput(1)]);

    layout.set_layout_mode(ContainerLayout::Tabbed);

    layout.add_window(
        TestWindow::new(TestWindowParams::new(1)),
        AddWindowTarget::Auto,
        None,
        None,
        false,
        ActivateWindow::default(),
    );
    layout.add_window(
        TestWindow::new(TestWindowParams::new(2)),
        AddWindowTarget::Auto,
        None,
        None,
        false,
        ActivateWindow::default(),
    );

    // Recorded: fixtures/layout-tabbed-on-an-empty-workspace.parity. sway makes the
    // *workspace* tabbed and both windows are its tabs; no container is built for them.
    let workspace = layout.active_workspace().expect("active workspace");
    let tree = workspace.container_tree().debug_tree();
    assert_eq!(workspace.debug_workspace_layout(), ContainerLayout::Tabbed);
    assert!(
        !workspace.container_tree().has_containers(),
        "a tabbed workspace holds its windows directly:\n{tree}",
    );
}
#[test]
fn i3_167_workspace_layout_stacked_groups_second_open() {
    let mut layout = Layout::default();
    check_ops_on_layout(&mut layout, [Op::AddOutput(1)]);

    layout.set_layout_mode(ContainerLayout::Stacked);

    layout.add_window(
        TestWindow::new(TestWindowParams::new(1)),
        AddWindowTarget::Auto,
        None,
        None,
        false,
        ActivateWindow::default(),
    );
    layout.add_window(
        TestWindow::new(TestWindowParams::new(2)),
        AddWindowTarget::Auto,
        None,
        None,
        false,
        ActivateWindow::default(),
    );

    let workspace = layout.active_workspace().expect("active workspace");
    let tree = workspace.container_tree().debug_tree();
    assert!(
        !workspace.container_tree().has_containers(),
        "a stacked workspace holds its windows directly:\n{tree}",
    );
    assert_eq!(workspace.debug_workspace_layout(), ContainerLayout::Stacked);
}
#[test]
fn i3_167_workspace_layout_stacked_reinserts_after_floating_roundtrip() {
    let mut layout = Layout::default();
    check_ops_on_layout(&mut layout, [Op::AddOutput(1)]);

    layout.set_layout_mode(ContainerLayout::Stacked);

    layout.add_window(
        TestWindow::new(TestWindowParams::new(1)),
        AddWindowTarget::Auto,
        None,
        None,
        false,
        ActivateWindow::default(),
    );
    layout.toggle_window_floating(None);
    layout.toggle_window_floating(None);
    layout.add_window(
        TestWindow::new(TestWindowParams::new(2)),
        AddWindowTarget::Auto,
        None,
        None,
        false,
        ActivateWindow::default(),
    );

    let workspace = layout.active_workspace().expect("active workspace");
    let tree = workspace.container_tree().debug_tree();
    assert!(
        !workspace.container_tree().has_containers(),
        "a stacked workspace holds its windows directly:\n{tree}",
    );
    assert_eq!(workspace.debug_workspace_layout(), ContainerLayout::Stacked);
}
#[test]
fn i3_167_empty_workspace_layout_can_switch_back_to_splith() {
    let mut layout = Layout::default();
    check_ops_on_layout(&mut layout, [Op::AddOutput(1)]);

    layout.set_layout_mode(ContainerLayout::Stacked);
    layout.add_window(
        TestWindow::new(TestWindowParams::new(1)),
        AddWindowTarget::Auto,
        None,
        None,
        false,
        ActivateWindow::default(),
    );
    layout.add_window(
        TestWindow::new(TestWindowParams::new(2)),
        AddWindowTarget::Auto,
        None,
        None,
        false,
        ActivateWindow::default(),
    );
    layout.remove_window(&1, Transaction::new());
    layout.remove_window(&2, Transaction::new());

    layout.set_layout_mode(ContainerLayout::SplitH);
    layout.add_window(
        TestWindow::new(TestWindowParams::new(3)),
        AddWindowTarget::Auto,
        None,
        None,
        false,
        ActivateWindow::default(),
    );
    layout.add_window(
        TestWindow::new(TestWindowParams::new(4)),
        AddWindowTarget::Auto,
        None,
        None,
        false,
        ActivateWindow::default(),
    );

    let workspace = layout.active_workspace().expect("active workspace");
    let tree = workspace.container_tree().debug_tree();
    assert!(
        !workspace.container_tree().contains_layout(ContainerLayout::Stacked),
        "after resetting empty workspace layout to splith, new opens should no longer land in stacked:\n{tree}",
    );
}
#[test]
fn i3_167_empty_workspace_layout_can_switch_back_to_splitv() {
    let mut layout = Layout::default();
    check_ops_on_layout(&mut layout, [Op::AddOutput(1)]);

    layout.set_layout_mode(ContainerLayout::Tabbed);
    layout.add_window(
        TestWindow::new(TestWindowParams::new(1)),
        AddWindowTarget::Auto,
        None,
        None,
        false,
        ActivateWindow::default(),
    );
    layout.add_window(
        TestWindow::new(TestWindowParams::new(2)),
        AddWindowTarget::Auto,
        None,
        None,
        false,
        ActivateWindow::default(),
    );
    layout.remove_window(&1, Transaction::new());
    layout.remove_window(&2, Transaction::new());

    layout.set_layout_mode(ContainerLayout::SplitV);
    layout.add_window(
        TestWindow::new(TestWindowParams::new(3)),
        AddWindowTarget::Auto,
        None,
        None,
        false,
        ActivateWindow::default(),
    );
    layout.add_window(
        TestWindow::new(TestWindowParams::new(4)),
        AddWindowTarget::Auto,
        None,
        None,
        false,
        ActivateWindow::default(),
    );

    let workspace = layout.active_workspace().expect("active workspace");
    let tree = workspace.container_tree().debug_tree();
    assert!(
        !workspace.container_tree().has_containers(),
        "the workspace carries the orientation itself:\n{tree}",
    );
    assert_eq!(workspace.debug_workspace_layout(), ContainerLayout::SplitV);
    assert!(
        !workspace.container_tree().contains_layout(ContainerLayout::Tabbed),
        "after resetting empty workspace layout to splitv, new opens should no longer land in tabbed:\n{tree}",
    );
}
#[test]
fn i3_101_directional_focus_on_single_window_is_noop() {
    let mut layout = check_ops([
        Op::AddOutput(1),
        Op::AddWindow {
            params: TestWindowParams::new(1),
        },
    ]);

    let focused_before = layout.focus().map(|win| *win.id());
    check_ops_on_layout(
        &mut layout,
        [
            Op::FocusWindowDown,
            Op::FocusWindowUp,
            Op::FocusColumnLeft,
            Op::FocusColumnRight,
        ],
    );

    assert_eq!(
        layout.focus().map(|win| *win.id()),
        focused_before,
        "directional focus should be a no-op when only one tiled window exists",
    );
}
#[test]
fn i3_121_focus_left_right_wraps_across_root_split() {
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

    assert_eq!(layout.focus().map(|win| *win.id()), Some(3));

    check_ops_on_layout(&mut layout, [Op::FocusColumnRight]);
    assert_eq!(
        layout.focus().map(|win| *win.id()),
        Some(1),
        "focus right should wrap from the rightmost root leaf to the leftmost one",
    );

    check_ops_on_layout(&mut layout, [Op::FocusColumnRight]);
    assert_eq!(layout.focus().map(|win| *win.id()), Some(2));

    check_ops_on_layout(&mut layout, [Op::FocusColumnRight]);
    assert_eq!(layout.focus().map(|win| *win.id()), Some(3));

    check_ops_on_layout(&mut layout, [Op::FocusColumnLeft]);
    assert_eq!(layout.focus().map(|win| *win.id()), Some(2));

    check_ops_on_layout(&mut layout, [Op::FocusColumnLeft]);
    assert_eq!(layout.focus().map(|win| *win.id()), Some(1));

    check_ops_on_layout(&mut layout, [Op::FocusColumnLeft]);
    assert_eq!(
        layout.focus().map(|win| *win.id()),
        Some(3),
        "focus left should wrap from the leftmost root leaf to the rightmost one",
    );
}
#[test]
fn i3_101_focus_window_command_targets_specific_leaf() {
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

    check_ops_on_layout(&mut layout, [Op::FocusWindow(2)]);
    assert_eq!(
        layout.focus().map(|win| *win.id()),
        Some(2),
        "focus command should activate the requested leaf directly",
    );

    check_ops_on_layout(&mut layout, [Op::FocusWindow(1)]);
    assert_eq!(layout.focus().map(|win| *win.id()), Some(1));
}
#[test]
fn i3_192_nested_container_layout_transitions() {
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
        Op::SetLayoutStacked,
    ]);
    let workspace = layout.active_workspace().expect("active workspace");
    let tree = workspace.container_tree().debug_tree();
    assert_snapshot!(
        tree.as_str(),
        @"
    SplitH
      Window 1
      Stacked
        Window 2
        Window 3 *
    "
    );
    check_ops_on_layout(&mut layout, [Op::SetLayoutTabbed]);
    let workspace = layout.active_workspace().expect("active workspace");
    let tree = workspace.container_tree().debug_tree();
    assert_snapshot!(
        tree.as_str(),
        @"
    SplitH
      Window 1
      Tabbed
        Window 2
        Window 3 *
    "
    );
    check_ops_on_layout(&mut layout, [Op::ToggleSplitLayout]);
    let workspace = layout.active_workspace().expect("active workspace");
    let tree = workspace.container_tree().debug_tree();
    assert_snapshot!(
        tree.as_str(),
        @"
    SplitH
      Window 1
      SplitV
        Window 2
        Window 3 *
    "
    );
}
#[test]
fn i3_192_toggle_layout_all_cycles_nested_container_layouts() {
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
        Op::ToggleLayoutAll,
    ]);
    let workspace = layout.active_workspace().expect("active workspace");
    let tree = workspace.container_tree().debug_tree();
    assert_snapshot!(
        tree.as_str(),
        @"
    SplitH
      Window 1
      Stacked
        Window 2
        Window 3 *
    "
    );
    check_ops_on_layout(&mut layout, [Op::ToggleLayoutAll]);
    let workspace = layout.active_workspace().expect("active workspace");
    let tree = workspace.container_tree().debug_tree();
    assert_snapshot!(
        tree.as_str(),
        @"
    SplitH
      Window 1
      Tabbed
        Window 2
        Window 3 *
    "
    );
    check_ops_on_layout(&mut layout, [Op::ToggleLayoutAll]);
    let workspace = layout.active_workspace().expect("active workspace");
    let tree = workspace.container_tree().debug_tree();
    assert_snapshot!(
        tree.as_str(),
        @"
    SplitH
      Window 1
      SplitH
        Window 2
        Window 3 *
    "
    );
}
#[test]
fn i3_192_nested_container_layout_sequence_matches_i3() {
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
        Op::SetLayoutStacked,
        Op::SetLayoutTabbed,
        Op::ToggleSplitLayout,
    ]);
    let workspace = layout.active_workspace().expect("active workspace");
    let tree = workspace.container_tree().debug_tree();
    assert_snapshot!(
        tree.as_str(),
        @"
    SplitH
      Window 1
      SplitV
        Window 2
        Window 3 *
    "
    );
    check_ops_on_layout(&mut layout, [Op::ToggleSplitLayout]);
    let workspace = layout.active_workspace().expect("active workspace");
    let tree = workspace.container_tree().debug_tree();
    assert_snapshot!(
        tree.as_str(),
        @"
    SplitH
      Window 1
      SplitH
        Window 2
        Window 3 *
    "
    );
}
#[test]
fn i3_122_repeated_split_on_single_window_does_not_nest_wrappers() {
    let mut layout = check_ops([
        Op::AddOutput(1),
        Op::AddWindow {
            params: TestWindowParams::new(1),
        },
        Op::SplitVertical,
    ]);
    let workspace = layout.active_workspace().expect("active workspace");
    let before = workspace.container_tree().debug_tree().replace(" *", "");
    check_ops_on_layout(&mut layout, [Op::SplitVertical]);
    let workspace = layout.active_workspace().expect("active workspace");
    let after = workspace.container_tree().debug_tree().replace(" *", "");
    assert_eq!(
        after, before,
        "repeating split on a single focused window should not keep nesting redundant wrappers",
    );
}
#[test]
fn i3_122_split_inside_stacked_creates_nested_split() {
    let layout = check_ops([
        Op::AddOutput(1),
        Op::AddWindow {
            params: TestWindowParams::new(1),
        },
        Op::SetLayoutStacked,
        Op::SplitHorizontal,
        Op::AddWindow {
            params: TestWindowParams::new(2),
        },
    ]);

    let workspace = layout.active_workspace().expect("active workspace");
    let tree = workspace.container_tree().debug_tree();
    assert_snapshot!(
        tree.as_str(),
        @"
    SplitH
      Stacked
        SplitH
          Window 1
          Window 2 *
    "
    );
}
#[test]
fn i3_122_toggle_split_switches_nested_container_orientation() {
    let layout = check_ops([
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
        Op::ToggleSplitLayout,
    ]);

    let workspace = layout.active_workspace().expect("active workspace");
    let tree = workspace.container_tree().debug_tree();
    assert_snapshot!(
        tree.as_str(),
        @"
    SplitH
      Window 1
      SplitH
        Window 2
        Window 3 *
    "
    );
}
#[test]
fn i3_122_split_workspace_with_multiple_children_wraps_focused_branch() {
    let layout = check_ops([
        Op::AddOutput(1),
        Op::AddWindow {
            params: TestWindowParams::new(1),
        },
        Op::AddWindow {
            params: TestWindowParams::new(2),
        },
        Op::FocusColumn(1),
        Op::SplitVertical,
        Op::AddWindow {
            params: TestWindowParams::new(3),
        },
    ]);
    let workspace = layout.active_workspace().expect("active workspace");
    let tree = workspace.container_tree().debug_tree();
    assert_snapshot!(
        tree.as_str(),
        @"
    SplitH
      SplitV
        Window 1
        Window 3 *
      Window 2
    "
    );
}
#[test]
fn i3_122_repeated_split_stops_once_the_window_is_alone() {
    // Recorded from sway (tiri-parity/fixtures/split-repeated-without-a-new-window.parity):
    // a split with siblings present always builds a container, even in the orientation the
    // parent already has. Repeating it is what does nothing, because by then the window is
    // alone in its own split and there is nothing left to separate it from.
    let mut layout = check_ops([
        Op::AddOutput(1),
        Op::AddWindow {
            params: TestWindowParams::new(1),
        },
        Op::AddWindow {
            params: TestWindowParams::new(2),
        },
        Op::FocusColumn(1),
        Op::SplitVertical,
        Op::AddWindow {
            params: TestWindowParams::new(3),
        },
    ]);

    check_ops_on_layout(&mut layout, [Op::SplitVertical]);
    let workspace = layout.active_workspace().expect("active workspace");
    let wrapped = workspace.container_tree().debug_tree().replace(" *", "");
    assert_eq!(
        wrapped.trim_end(),
        "SplitH\n  SplitV\n    Window 1\n    SplitV\n      Window 3\n  Window 2",
        "a split with a sibling present builds a container",
    );

    check_ops_on_layout(&mut layout, [Op::SplitVertical]);
    let workspace = layout.active_workspace().expect("active workspace");
    assert_eq!(
        workspace
            .container_tree()
            .debug_tree()
            .replace(" *", "")
            .trim_end(),
        wrapped.trim_end(),
        "repeating it once the window is alone in its split does nothing",
    );
}
#[test]
fn i3_122_split_on_empty_workspace_applies_to_next_window() {
    // i3 #122: `split v` on an empty workspace sets the workspace orientation. The first
    // window is a plain child of the workspace — no wrapper of its own — and the
    // orientation shows once a second window arrives.
    let mut layout = check_ops([
        Op::AddOutput(1),
        Op::SplitVertical,
        Op::AddWindow {
            params: TestWindowParams::new(1),
        },
    ]);

    {
        let workspace = layout.active_workspace().expect("active workspace");
        assert_snapshot!(
            workspace.container_tree().debug_tree().as_str(),
            @r"
        SplitV
          Window 1 *
        "
        );
    }

    check_ops_on_layout(
        &mut layout,
        [Op::AddWindow {
            params: TestWindowParams::new(2),
        }],
    );

    let workspace = layout.active_workspace().expect("active workspace");
    assert_snapshot!(
        workspace.container_tree().debug_tree().as_str(),
        @"
    SplitV
      Window 1
      Window 2 *
    "
    );
}
#[test]
fn i3_122_split_on_single_window_persists_after_close() {
    // i3 #122: the workspace orientation outlives the windows that were arranged by it.
    let layout = check_ops([
        Op::AddOutput(1),
        Op::AddWindow {
            params: TestWindowParams::new(1),
        },
        Op::SplitVertical,
        Op::CloseWindow(1),
        Op::AddWindow {
            params: TestWindowParams::new(2),
        },
        Op::AddWindow {
            params: TestWindowParams::new(3),
        },
    ]);

    let workspace = layout.active_workspace().expect("active workspace");
    assert_snapshot!(
        workspace.container_tree().debug_tree().as_str(),
        @"
    SplitV
      Window 2
      Window 3 *
    "
    );
}
#[test]
fn i3_124_move_single_window_is_noop() {
    let mut layout = check_ops([
        Op::AddOutput(1),
        Op::AddWindow {
            params: TestWindowParams::new(1),
        },
    ]);
    let workspace = layout.active_workspace().expect("active workspace");
    let before = workspace.container_tree().debug_tree();
    check_ops_on_layout(
        &mut layout,
        [
            Op::MoveColumnLeft,
            Op::MoveColumnRight,
            Op::MoveWindowUp,
            Op::MoveWindowDown,
        ],
    );
    // Recorded: fixtures/move-a-lone-window.parity. Nothing shifts — there is nothing to
    // shift past — but the workspace turns to face each move, so the last one decides.
    let workspace = layout.active_workspace().expect("active workspace");
    let after = workspace.container_tree().debug_tree();
    assert_eq!(workspace.debug_workspace_layout(), ContainerLayout::SplitV);
    assert_eq!(
        after.replace("SplitV", "SplitH"),
        before,
        "a lone window stays where it is:\n{after}",
    );
}
#[test]
fn i3_124_move_window_into_adjacent_split_container() {
    let layout = check_ops([
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
        Op::FocusColumnLeft,
        Op::MoveColumnRight,
    ]);

    let workspace = layout.active_workspace().expect("active workspace");
    let tree = workspace.container_tree().debug_tree();
    assert_snapshot!(
        tree.as_str(),
        @"
    SplitH
      SplitV
        Window 2
        Window 1 *
        Window 3
    "
    );
}
#[test]
fn i3_124_move_window_out_of_split_on_layout_mismatch() {
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
        Op::MoveColumnRight,
    ]);

    let workspace = layout.active_workspace().expect("active workspace");
    let tree = workspace.container_tree().debug_tree();
    assert_snapshot!(
        tree.as_str(),
        @"
    SplitH
      SplitV
        Window 1
      Window 3 *
      Window 2
    "
    );
}

#[test]
fn i3_124_move_container_right_moves_focused_leaf_out_of_nested_split() {
    let mut layout = check_ops([
        Op::AddOutput(1),
        Op::AddWindow {
            params: TestWindowParams::new(1),
        },
        Op::AddWindow {
            params: TestWindowParams::new(2),
        },
        Op::FocusWindow(1),
        Op::SplitVertical,
        Op::AddWindow {
            params: TestWindowParams::new(3),
        },
    ]);

    layout.move_container_right();
    layout.verify_invariants();

    let workspace = layout.active_workspace().expect("active workspace");
    let tree = workspace.container_tree().debug_tree();
    assert_snapshot!(
        tree.as_str(),
        @"
    SplitH
      SplitV
        Window 1
      Window 3 *
      Window 2
    "
    );
}

#[test]
fn i3_145_move_up_then_right_flattens_back_to_root_siblings() {
    let layout = check_ops([
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
        Op::MoveWindowUp,
        Op::MoveColumnRight,
    ]);

    let workspace = layout.active_workspace().expect("active workspace");
    let tree = workspace.container_tree().debug_tree();
    assert_snapshot!(
        tree.as_str(),
        @"
    SplitH
      Window 2
      Window 1
      Window 3 *
    "
    );
}
#[test]
fn i3_145_ticket_1053_sequence_flattens_after_second_move() {
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
        Op::AddWindow {
            params: TestWindowParams::new(4),
        },
        Op::FocusColumnRight,
        Op::SplitVertical,
        Op::FocusColumnRight,
        Op::MoveColumnLeft,
        Op::SetLayoutTabbed,
        Op::FocusParent,
        Op::SplitVertical,
    ]);
    let workspace = layout.active_workspace().expect("active workspace");
    let before = workspace.container_tree().debug_tree();
    assert_eq!(
        workspace.container_tree().root_children_len(),
        3,
        "precondition: first phase of i3 145 ticket #1053 should still have 3 root children:\n{before}",
    );
    check_ops_on_layout(&mut layout, [Op::FocusColumnRight, Op::MoveColumnLeft]);
    let workspace = layout.active_workspace().expect("active workspace");
    let after = workspace.container_tree().debug_tree();
    assert_eq!(
        workspace.container_tree().root_children_len(),
        2,
        "i3 145 ticket #1053 should flatten redundant wrappers after the second move:\n{after}",
    );
}
#[test]
fn i3_104_focus_stack_restores_tiling_focus_after_floating_close() {
    // Mirrors i3_test_cases/t/104-focus-stack.t:
    // opening a floating window must not lose the previous tiling focus after close.
    let mut floating = TestWindowParams::new(3);
    floating.is_floating = true;

    let layout = check_ops([
        Op::AddOutput(1),
        Op::AddWindow {
            params: TestWindowParams::new(1),
        },
        Op::AddWindow {
            params: TestWindowParams::new(2),
        },
        Op::AddWindow { params: floating },
        Op::CloseWindow(3),
    ]);

    let focused = layout
        .focus()
        .map(|win| *win.id())
        .expect("focused window should exist");
    assert_eq!(
        focused, 2,
        "focus should restore to previously-focused tiled window"
    );
}
#[test]
fn i3_117_workspace_previous_switches_between_mru_workspaces() {
    let mut layout = check_ops([
        Op::AddOutput(1),
        Op::AddWindow {
            params: TestWindowParams::new(1),
        },
        Op::AddWindow {
            params: TestWindowParams::new(2),
        },
        Op::FocusWindow(1),
        Op::MoveWindowToWorkspaceDown(true),
    ]);

    {
        let workspace = layout.active_workspace().expect("active workspace");
        assert!(
            workspace.has_window(&1),
            "moving with focus=true should leave us on the destination workspace",
        );
    }

    check_ops_on_layout(&mut layout, [Op::FocusWorkspacePrevious]);

    {
        let workspace = layout.active_workspace().expect("active workspace");
        assert!(
            workspace.has_window(&2),
            "workspace previous should restore the previously-focused workspace",
        );
        assert!(!workspace.has_window(&1));
    }

    check_ops_on_layout(&mut layout, [Op::FocusWorkspacePrevious]);

    let workspace = layout.active_workspace().expect("active workspace");
    assert!(
        workspace.has_window(&1),
        "workspace previous should toggle back to the MRU workspace",
    );
}
#[test]
fn i3_118_open_then_kill_single_window_leaves_workspace_empty() {
    let mut layout = check_ops([
        Op::AddOutput(1),
        Op::AddWindow {
            params: TestWindowParams::new(1),
        },
    ]);

    assert!(layout.has_window(&1));
    layout.remove_window(&1, Transaction::new());

    let workspace = layout.active_workspace().expect("active workspace");
    assert_eq!(workspace.windows().count(), 0);
    assert_eq!(workspace.container_tree().tiles().count(), 0);
    assert_eq!(workspace.floating().tiles().count(), 0);
}
#[test]
fn i3_118_kill_unfocused_window_by_id_removes_correct_leaf() {
    let mut layout = check_ops([
        Op::AddOutput(1),
        Op::AddWindow {
            params: TestWindowParams::new(1),
        },
        Op::AddWindow {
            params: TestWindowParams::new(2),
        },
    ]);

    assert_eq!(layout.focus().map(|win| *win.id()), Some(2));
    layout.remove_window(&1, Transaction::new());

    let workspace = layout.active_workspace().expect("active workspace");
    assert!(!workspace.has_window(&1));
    assert!(workspace.has_window(&2));
    assert_eq!(layout.focus().map(|win| *win.id()), Some(2));
}
#[test]
fn i3_129_focus_after_close_prefers_focus_stack_leaf() {
    // Mirrors i3_test_cases/t/129-focus-after-close.t (second scenario):
    // when closing an active leaf, focus is restored to the most-recent leaf from the stack.
    let layout = check_ops([
        Op::AddOutput(1),
        Op::SplitVertical,
        Op::AddWindow {
            params: TestWindowParams::new(1),
        },
        Op::AddWindow {
            params: TestWindowParams::new(2),
        },
        Op::FocusWindowUp,
        Op::SplitHorizontal,
        Op::AddWindow {
            params: TestWindowParams::new(3),
        },
        Op::AddWindow {
            params: TestWindowParams::new(4),
        },
        Op::FocusWindowDown,
        Op::CloseWindow(2),
    ]);

    let focused = layout
        .focus()
        .map(|win| *win.id())
        .expect("focused window should exist");
    assert_eq!(
        focused, 4,
        "after closing the bottom leaf, focus should return to top-right MRU leaf",
    );
}
#[test]
fn i3_129_kill_workspace_closes_tiling_and_floating_windows() {
    let mut layout = check_ops([
        Op::AddOutput(1),
        Op::AddWindow {
            params: TestWindowParams::new(1),
        },
        Op::AddWindow {
            params: TestWindowParams::new(2),
        },
        Op::ToggleWindowFloating { id: None },
        Op::FocusParent,
        Op::FocusParent,
    ]);

    let selected_ids = layout.close_window_ids_for_active_selection();
    assert_eq!(
        selected_ids.len(),
        2,
        "workspace-level kill should target both tiling and floating windows",
    );

    for id in selected_ids {
        layout.remove_window(&id, Transaction::new());
    }

    let workspace = layout.active_workspace().expect("active workspace");
    assert_eq!(workspace.windows().count(), 0);
    assert_eq!(workspace.container_tree().tiles().count(), 0);
    assert_eq!(workspace.floating().tiles().count(), 0);
}
#[test]
fn i3_130_closing_last_children_removes_empty_split_wrapper() {
    let layout = check_ops([
        Op::AddOutput(1),
        Op::AddWindow {
            params: TestWindowParams::new(1),
        },
        Op::AddWindow {
            params: TestWindowParams::new(2),
        },
        Op::FocusWindow(1),
        Op::SplitVertical,
        Op::AddWindow {
            params: TestWindowParams::new(3),
        },
        Op::CloseWindow(3),
        Op::CloseWindow(1),
    ]);

    let workspace = layout.active_workspace().expect("active workspace");
    let tree = workspace.container_tree().debug_tree();
    assert_snapshot!(
        tree.as_str(),
        @r"
    SplitH
      Window 2 *
    "
    );
}
#[test]
fn i3_130_moving_last_children_away_removes_empty_split_wrapper() {
    let layout = check_ops([
        Op::AddOutput(1),
        Op::AddWindow {
            params: TestWindowParams::new(1),
        },
        Op::AddWindow {
            params: TestWindowParams::new(2),
        },
        Op::FocusWindow(1),
        Op::SplitVertical,
        Op::AddWindow {
            params: TestWindowParams::new(3),
        },
        Op::MoveWindowToWorkspaceDown(false),
        Op::FocusWindow(1),
        Op::MoveWindowToWorkspaceDown(false),
    ]);

    let workspace = layout.active_workspace().expect("active workspace");
    assert!(workspace.has_window(&2));
    assert!(!workspace.has_window(&1));
    assert!(!workspace.has_window(&3));

    let tree = workspace.container_tree().debug_tree();
    assert_snapshot!(
        tree.as_str(),
        @r"
    SplitH
      Window 2 *
    "
    );
}
#[test]
fn i3_124_move_left_then_right_swaps_root_siblings_without_extra_changes() {
    let mut layout = check_ops([
        Op::AddOutput(1),
        Op::AddWindow {
            params: TestWindowParams::new(1),
        },
        Op::AddWindow {
            params: TestWindowParams::new(2),
        },
        Op::MoveColumnLeft,
    ]);
    let workspace = layout.active_workspace().expect("active workspace");
    let after_left = workspace.container_tree().debug_tree();
    assert_eq!(
        workspace.container_tree().all_window_ids(),
        vec![2, 1],
        "moving the second root sibling left should swap it before the first:\n{after_left}",
    );
    assert_eq!(
        workspace.container_tree().focused_window_id(),
        Some(2),
        "moving the second root sibling left should swap it before the first:\n{after_left}",
    );
    check_ops_on_layout(&mut layout, [Op::MoveColumnLeft]);
    let workspace = layout.active_workspace().expect("active workspace");
    let after_second_left = workspace.container_tree().debug_tree();
    assert_eq!(
        workspace.container_tree().all_window_ids(),
        vec![2, 1],
        "moving left again at the edge should be a no-op:\n{after_second_left}",
    );
    assert_eq!(
        workspace.container_tree().focused_window_id(),
        Some(2),
        "moving left again at the edge should be a no-op:\n{after_second_left}",
    );
    check_ops_on_layout(&mut layout, [Op::MoveColumnRight]);
    let workspace = layout.active_workspace().expect("active workspace");
    let after_right = workspace.container_tree().debug_tree();
    assert_eq!(
        workspace.container_tree().all_window_ids(),
        vec![1, 2],
        "moving right should swap the root siblings back:\n{after_right}",
    );
    assert_eq!(
        workspace.container_tree().focused_window_id(),
        Some(2),
        "moving right should swap the root siblings back:\n{after_right}",
    );
    check_ops_on_layout(&mut layout, [Op::MoveColumnRight]);
    let workspace = layout.active_workspace().expect("active workspace");
    let after_second_right = workspace.container_tree().debug_tree();
    assert_eq!(
        workspace.container_tree().all_window_ids(),
        vec![1, 2],
        "moving right again at the edge should be a no-op:\n{after_second_right}",
    );
    assert_eq!(
        workspace.container_tree().focused_window_id(),
        Some(2),
        "moving right again at the edge should be a no-op:\n{after_second_right}",
    );
}
#[test]
fn i3_124_moving_all_children_out_of_split_removes_source_container() {
    let layout = check_ops([
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
        Op::FocusWindow(1),
        Op::SplitVertical,
        Op::AddWindow {
            params: TestWindowParams::new(4),
        },
        Op::FocusWindow(4),
        Op::MoveColumnRight,
        Op::FocusWindow(1),
        Op::MoveColumnRight,
    ]);

    let workspace = layout.active_workspace().expect("active workspace");
    let tree = workspace.container_tree().debug_tree();
    let mut ids = workspace.container_tree().all_window_ids();
    ids.sort_unstable();

    assert_eq!(
        workspace.container_tree().root_children_len(),
        1,
        "after moving the last two children out of the left split, the source container should be removed:\n{tree}",
    );
    assert_eq!(
        ids,
        vec![1, 2, 3, 4],
        "all windows should still be present:\n{tree}"
    );
}
#[test]
fn i3_127_killing_parent_chain_then_disabling_floating_reinserts_cleanly() {
    let layout = check_ops([
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
        Op::ToggleWindowFloating { id: None },
        Op::FocusWindow(2),
        Op::CloseWindow(2),
        Op::FocusWindow(1),
        Op::CloseWindow(1),
        Op::FocusWindow(3),
        Op::ToggleWindowFloating { id: None },
    ]);

    let workspace = layout.active_workspace().expect("active workspace");
    assert_eq!(workspace.floating().tiles().count(), 0);
    assert_eq!(workspace.container_tree().tiles().count(), 1);
    assert_snapshot!(
        workspace.container_tree().debug_tree().as_str(),
        @r"
    SplitH
      Window 3 *
    "
    );
}
#[test]
fn i3_135_floating_toggle_roundtrip_preserves_focus() {
    let layout = check_ops([
        Op::AddOutput(1),
        Op::AddWindow {
            params: TestWindowParams::new(1),
        },
        Op::AddWindow {
            params: TestWindowParams::new(2),
        },
        Op::ToggleWindowFloating { id: None },
        Op::ToggleWindowFloating { id: None },
    ]);

    assert_eq!(
        layout.focus().map(|win| *win.id()),
        Some(2),
        "toggling the focused window to floating and back should preserve focus",
    );
}
#[test]
fn i3_135_killing_unfocused_floating_window_keeps_current_floating_focus() {
    let layout = check_ops([
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
        Op::ToggleWindowFloating { id: Some(3) },
        Op::FocusWindow(2),
        Op::ToggleWindowFloating { id: None },
        Op::CloseWindow(3),
    ]);

    let workspace = layout.active_workspace().expect("active workspace");
    assert_eq!(layout.focus().map(|win| *win.id()), Some(2));
    assert!(workspace.is_floating(&2));
    assert!(!workspace.has_window(&3));
}
#[test]
fn i3_135_killing_focused_floating_window_falls_back_to_next_floating_then_tiling() {
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
        Op::FocusWindow(2),
        Op::ToggleWindowFloating { id: None },
        Op::FocusWindow(3),
        Op::ToggleWindowFloating { id: None },
        Op::FocusWindow(2),
    ]);

    check_ops_on_layout(&mut layout, [Op::CloseWindow(2)]);
    assert_eq!(
        layout.focus().map(|win| *win.id()),
        Some(3),
        "after closing the focused floating window, focus should fall back to the next floating window",
    );

    check_ops_on_layout(&mut layout, [Op::CloseWindow(3)]);
    assert_eq!(
        layout.focus().map(|win| *win.id()),
        Some(1),
        "after closing the last floating window, focus should fall back to the last tiled window",
    );
}
#[test]
fn i3_135_focus_tiling_focus_floating_and_mode_toggle_switch_domains() {
    let mut layout = check_ops([
        Op::AddOutput(1),
        Op::AddWindow {
            params: TestWindowParams::new(1),
        },
        Op::AddWindow {
            params: TestWindowParams::new(2),
        },
        Op::ToggleWindowFloating { id: None },
    ]);

    check_ops_on_layout(&mut layout, [Op::FocusTiling]);
    assert_eq!(layout.focus().map(|win| *win.id()), Some(1));

    check_ops_on_layout(&mut layout, [Op::FocusFloating]);
    assert_eq!(layout.focus().map(|win| *win.id()), Some(2));

    check_ops_on_layout(&mut layout, [Op::FocusFloating]);
    assert_eq!(
        layout.focus().map(|win| *win.id()),
        Some(2),
        "focus floating on an already-focused floating window should be a no-op",
    );

    check_ops_on_layout(&mut layout, [Op::SwitchFocusFloatingTiling]);
    assert_eq!(layout.focus().map(|win| *win.id()), Some(1));

    check_ops_on_layout(&mut layout, [Op::SwitchFocusFloatingTiling]);
    assert_eq!(layout.focus().map(|win| *win.id()), Some(2));
}
#[test]
fn i3_135_directional_focus_cycles_across_floating_windows() {
    let mut one = TestWindowParams::new(1);
    one.is_floating = true;
    let mut two = TestWindowParams::new(2);
    two.is_floating = true;
    let mut three = TestWindowParams::new(3);
    three.is_floating = true;

    let mut layout = check_ops([
        Op::AddOutput(1),
        Op::AddWindow { params: one },
        Op::AddWindow { params: two },
        Op::AddWindow { params: three },
    ]);

    assert_eq!(layout.focus().map(|win| *win.id()), Some(3));

    check_ops_on_layout(&mut layout, [Op::FocusColumnLeft]);
    assert_eq!(layout.focus().map(|win| *win.id()), Some(2));

    check_ops_on_layout(&mut layout, [Op::FocusColumnLeft]);
    assert_eq!(layout.focus().map(|win| *win.id()), Some(1));

    check_ops_on_layout(&mut layout, [Op::FocusColumnLeft]);
    assert_eq!(layout.focus().map(|win| *win.id()), Some(3));

    check_ops_on_layout(&mut layout, [Op::FocusColumnRight]);
    assert_eq!(layout.focus().map(|win| *win.id()), Some(1));

    check_ops_on_layout(&mut layout, [Op::FocusColumnRight]);
    assert_eq!(layout.focus().map(|win| *win.id()), Some(2));
}
#[test]
fn i3_135_focusing_floating_window_raises_it_to_front() {
    let mut one = TestWindowParams::new(1);
    one.is_floating = true;
    let mut two = TestWindowParams::new(2);
    two.is_floating = true;

    let mut layout = check_ops([
        Op::AddOutput(1),
        Op::AddWindow { params: one },
        Op::AddWindow { params: two },
    ]);

    {
        let workspace = layout.active_workspace().expect("active workspace");
        let order: Vec<_> = workspace
            .floating()
            .tiles()
            .map(|tile| *tile.window().id())
            .collect();
        assert_eq!(
            order.first().copied(),
            Some(2),
            "precondition: newest floating window should start on top",
        );
    }

    check_ops_on_layout(&mut layout, [Op::FocusWindow(1)]);

    let workspace = layout.active_workspace().expect("active workspace");
    let order: Vec<_> = workspace
        .floating()
        .tiles()
        .map(|tile| *tile.window().id())
        .collect();
    assert_eq!(layout.focus().map(|win| *win.id()), Some(1));
    assert_eq!(
        order.first().copied(),
        Some(1),
        "focusing a floating window should raise its container to the top of the floating stack",
    );
}
#[test]
fn i3_135_toggle_floating_on_focused_window_from_other_workspace_preserves_focus() {
    let mut layout = check_ops([
        Op::AddOutput(1),
        Op::AddWindow {
            params: TestWindowParams::new(1),
        },
        Op::AddWindow {
            params: TestWindowParams::new(2),
        },
        Op::FocusWorkspace(1),
        Op::ToggleWindowFloating { id: Some(2) },
        Op::FocusWorkspace(0),
    ]);

    let workspace = layout.active_workspace().expect("active workspace");
    assert!(workspace.is_floating(&2));
    assert_eq!(
        layout.focus().map(|win| *win.id()),
        Some(2),
        "toggling floating for the focused window from another workspace should preserve its focus",
    );

    check_ops_on_layout(
        &mut layout,
        [
            Op::FocusWorkspace(1),
            Op::ToggleWindowFloating { id: Some(2) },
            Op::FocusWorkspace(0),
        ],
    );

    let workspace = layout.active_workspace().expect("active workspace");
    assert!(!workspace.is_floating(&2));
    assert_eq!(
        layout.focus().map(|win| *win.id()),
        Some(2),
        "toggling the same window back to tiling from another workspace should still preserve focus",
    );
}
#[test]
fn i3_135_toggle_floating_on_unfocused_window_from_other_workspace_does_not_steal_focus() {
    let mut layout = check_ops([
        Op::AddOutput(1),
        Op::AddWindow {
            params: TestWindowParams::new(1),
        },
        Op::AddWindow {
            params: TestWindowParams::new(2),
        },
        Op::FocusWorkspace(1),
        Op::ToggleWindowFloating { id: Some(1) },
        Op::FocusWorkspace(0),
    ]);

    let workspace = layout.active_workspace().expect("active workspace");
    assert!(workspace.is_floating(&1));
    assert_eq!(
        layout.focus().map(|win| *win.id()),
        Some(2),
        "toggling floating for an unfocused window from another workspace must not steal focus",
    );

    check_ops_on_layout(
        &mut layout,
        [
            Op::FocusWorkspace(1),
            Op::ToggleWindowFloating { id: Some(1) },
            Op::FocusWorkspace(0),
        ],
    );

    let workspace = layout.active_workspace().expect("active workspace");
    assert!(!workspace.is_floating(&1));
    assert_eq!(
        layout.focus().map(|win| *win.id()),
        Some(2),
        "toggling the unfocused window back to tiling from another workspace must still not steal focus",
    );
}
#[test]
fn i3_135_toggle_floating_on_other_workspace_keeps_focused_floating_window() {
    let mut layout = check_ops([
        Op::AddOutput(1),
        Op::AddWindow {
            params: TestWindowParams::new(1),
        },
        Op::AddWindow {
            params: TestWindowParams::new(2),
        },
        Op::ToggleWindowFloating { id: Some(2) },
        Op::AddWindow {
            params: TestWindowParams::new(3),
        },
        Op::FocusWorkspace(1),
        Op::ToggleWindowFloating { id: Some(3) },
        Op::FocusWorkspace(0),
    ]);

    let workspace = layout.active_workspace().expect("active workspace");
    assert!(workspace.is_floating(&2));
    assert!(workspace.is_floating(&3));
    assert_eq!(
        layout.focus().map(|win| *win.id()),
        Some(3),
        "if the toggled window was focused on its workspace, it should remain focused after returning",
    );

    check_ops_on_layout(
        &mut layout,
        [
            Op::FocusWorkspace(1),
            Op::ToggleWindowFloating { id: Some(3) },
            Op::FocusWorkspace(0),
        ],
    );

    let workspace = layout.active_workspace().expect("active workspace");
    assert!(workspace.is_floating(&2));
    assert!(!workspace.is_floating(&3));
    assert_eq!(
        layout.focus().map(|win| *win.id()),
        Some(3),
        "toggling that same focused window back to tiling on another workspace should keep focus on it",
    );
}
#[test]
fn i3_135_toggle_unfocused_window_on_other_workspace_keeps_current_floating_focus() {
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
        Op::ToggleWindowFloating { id: Some(3) },
        Op::FocusWorkspace(1),
        Op::ToggleWindowFloating { id: Some(2) },
        Op::FocusWorkspace(0),
    ]);

    let workspace = layout.active_workspace().expect("active workspace");
    assert!(workspace.is_floating(&2));
    assert!(workspace.is_floating(&3));
    assert_eq!(
        layout.focus().map(|win| *win.id()),
        Some(3),
        "toggling another window from a different workspace must not steal focus from the current floating window",
    );

    check_ops_on_layout(
        &mut layout,
        [
            Op::FocusWorkspace(1),
            Op::ToggleWindowFloating { id: Some(2) },
            Op::FocusWorkspace(0),
        ],
    );

    let workspace = layout.active_workspace().expect("active workspace");
    assert!(!workspace.is_floating(&2));
    assert!(workspace.is_floating(&3));
    assert_eq!(
        layout.focus().map(|win| *win.id()),
        Some(3),
        "toggling the unfocused window back to tiling on another workspace must keep floating focus unchanged",
    );
}
#[test]
fn i3_135_toggle_floating_for_nested_window_from_other_workspace_preserves_focus() {
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
        Op::FocusWorkspace(1),
        Op::ToggleWindowFloating { id: Some(3) },
        Op::FocusWorkspace(0),
    ]);

    {
        let workspace = layout.active_workspace().expect("active workspace");
        assert!(workspace.is_floating(&3));
        assert_eq!(
            layout.focus().map(|win| *win.id()),
            Some(3),
            "toggling a focused nested window to floating from another workspace should preserve its focus",
        );
    }

    check_ops_on_layout(
        &mut layout,
        [
            Op::FocusWorkspace(1),
            Op::ToggleWindowFloating { id: Some(3) },
            Op::FocusWorkspace(0),
        ],
    );

    let workspace = layout.active_workspace().expect("active workspace");
    assert!(!workspace.is_floating(&3));
    assert_eq!(
        layout.focus().map(|win| *win.id()),
        Some(3),
        "toggling that nested window back to tiling from another workspace should still preserve focus",
    );
    let tree = workspace.container_tree().debug_tree();
    assert!(
        workspace.container_tree().focused_window_id() == Some(3),
        "after the roundtrip, the nested window should still be the focused tiling leaf:\n{tree}",
    );
}
#[test]
fn i3_135_deep_floating_roundtrip_from_other_workspace_preserves_focus_chain() {
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
        Op::SplitHorizontal,
        Op::AddWindow {
            params: TestWindowParams::new(4),
        },
        Op::SplitVertical,
        Op::AddWindow {
            params: TestWindowParams::new(5),
        },
    ]);

    let tree_before = layout
        .active_workspace()
        .expect("active workspace")
        .container_tree()
        .debug_tree()
        .replace(" *", "");
    assert!(
        tree_before.contains("SplitV\n        Window 4\n        Window 5"),
        "precondition: deep nested layout should place window 4 before 5 in the innermost split:\n{tree_before}",
    );
    assert_eq!(layout.focus().map(|win| *win.id()), Some(5));

    check_ops_on_layout(
        &mut layout,
        [
            Op::FocusWorkspace(1),
            Op::ToggleWindowFloating { id: Some(4) },
            Op::FocusWorkspace(0),
        ],
    );

    {
        let workspace = layout.active_workspace().expect("active workspace");
        assert!(workspace.is_floating(&4));
        assert_eq!(
            layout.focus().map(|win| *win.id()),
            Some(5),
            "after floating the deep nested window from another workspace, focus should stay on D-like sibling",
        );
        let tree = workspace.container_tree().debug_tree().replace(" *", "");
        assert!(
            workspace.container_tree().windows().any(|win| *win.id() == 5),
            "the tiling tree should keep the sibling that replaced the floated window's slot:\n{tree}",
        );
    }

    check_ops_on_layout(
        &mut layout,
        [
            Op::FocusWorkspace(1),
            Op::ToggleWindowFloating { id: Some(4) },
            Op::FocusWorkspace(0),
        ],
    );

    {
        let workspace = layout.active_workspace().expect("active workspace");
        assert!(!workspace.is_floating(&4));
        assert_eq!(
            layout.focus().map(|win| *win.id()),
            Some(5),
            "after restoring the deep nested window to tiling from another workspace, focus should stay on the previously-focused sibling",
        );
        let tree = workspace.container_tree().debug_tree();
        assert!(
            workspace.container_tree().windows().any(|win| *win.id() == 4) && workspace.container_tree().focused_window_id() == Some(5),
            "after the roundtrip both deep siblings should exist and window 5 should still be focused:\n{tree}",
        );
    }

    check_ops_on_layout(&mut layout, [Op::CloseWindow(5)]);
    // sway hands focus to `seat_get_focus_inactive` when the focused view goes. Floating has
    // only detached and reattached window 4's node, so its place in that order still answers
    // before window 3. Tiri now does the same in one workspace tree: `float_subtree` moves the
    // same NodeKey between branches instead of rebuilding it in another arena.
    //
    // sway/tree/container.c:1004-1059
    assert_eq!(
        layout.focus().map(|win| *win.id()),
        Some(4),
        "the floated-and-restored node keeps its place in the workspace focus order",
    );

    check_ops_on_layout(&mut layout, [Op::CloseWindow(4)]);
    assert_eq!(
        layout.focus().map(|win| *win.id()),
        Some(3),
        "after killing the restored deep window, focus should move up the focus stack to the next ancestor leaf",
    );

    check_ops_on_layout(&mut layout, [Op::CloseWindow(3)]);
    assert_eq!(
        layout.focus().map(|win| *win.id()),
        Some(2),
        "after killing the next leaf, focus should continue restoring toward the previous sibling branch",
    );

    check_ops_on_layout(&mut layout, [Op::CloseWindow(2)]);
    assert_eq!(
        layout.focus().map(|win| *win.id()),
        Some(1),
        "after killing that branch, the root-left leaf should receive focus",
    );
}
#[test]
fn i3_135_focus_parent_then_focus_child_roundtrips_from_floating_window() {
    let mut floating = TestWindowParams::new(2);
    floating.is_floating = true;

    let mut layout = check_ops([
        Op::AddOutput(1),
        Op::AddWindow {
            params: TestWindowParams::new(1),
        },
        Op::AddWindow { params: floating },
    ]);

    check_ops_on_layout(&mut layout, [Op::FocusParent]);
    {
        let workspace = layout.active_workspace().expect("active workspace");
        assert!(
            workspace.debug_floating_workspace_context(),
            "focus parent from a floating window should move to workspace context",
        );
    }

    check_ops_on_layout(&mut layout, [Op::FocusChild]);
    assert_eq!(
        layout.focus().map(|win| *win.id()),
        Some(2),
        "focus child from workspace context should return to the floating window",
    );
}
#[test]
fn i3_146_floating_toggle_reinserts_into_previous_split_container() {
    // Mirrors i3_test_cases/t/146-floating-reinsert.t:
    // toggling a floating window back to tiling should reinsert it in the focused split.
    let mut floating = TestWindowParams::new(4);
    floating.is_floating = true;

    let layout = check_ops([
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
        Op::AddWindow { params: floating },
        Op::ToggleWindowFloating { id: None },
    ]);

    let workspace = layout.active_workspace().expect("active workspace");
    assert_eq!(workspace.floating().tiles().count(), 0);
    assert_eq!(workspace.container_tree().tiles().count(), 4);
    assert_snapshot!(
        workspace.container_tree().debug_tree().as_str(),
        @"
    SplitH
      Window 1
      SplitV
        Window 2
        Window 3
        Window 4 *
    "
    );
}
#[test]
fn i3_152_focus_parent_then_toggle_floating_workspace_context_behaves_like_sway() {
    // Mirrors i3_test_cases/t/152-regress-level-up.t and extends it with a
    // sway-equivalent no-op check when toggling from workspace context with empty tiling.
    let mut layout = check_ops([
        Op::AddOutput(1),
        Op::AddWindow {
            params: TestWindowParams::new(1),
        },
        Op::FocusParent,
        Op::FocusParent,
        Op::ToggleWindowFloating { id: None },
    ]);

    {
        let workspace = layout.active_workspace().expect("active workspace");
        assert!(workspace.floating_is_active());
        assert_eq!(workspace.container_tree().tiles().count(), 0);
        assert_eq!(workspace.floating().tiles().count(), 1);
    }

    check_ops_on_layout(
        &mut layout,
        [Op::FocusParent, Op::ToggleWindowFloating { id: None }],
    );

    let workspace = layout.active_workspace().expect("active workspace");
    assert!(
        workspace.floating_is_active(),
        "workspace-context toggle_floating with empty tiling must be a no-op",
    );
    assert_eq!(workspace.container_tree().tiles().count(), 0);
    assert_eq!(workspace.floating().tiles().count(), 1);
}
#[test]
fn i3_218_floating_container_cannot_be_split_or_relayouted() {
    // Mirrors i3_test_cases/t/218-regress-floating-split.t:
    // layout on a floating leaf is a no-op; split creates one explicit split wrapper.
    let mut params = TestWindowParams::new(1);
    params.is_floating = true;

    let mut layout = check_ops([Op::AddOutput(1), Op::AddWindow { params }]);

    let before_layout = layout
        .active_workspace()
        .expect("active workspace")
        .floating()
        .root_layout_for_window(&1);

    check_ops_on_layout(&mut layout, [Op::SetLayoutStacked]);

    let after_layout = layout
        .active_workspace()
        .expect("active workspace")
        .floating()
        .root_layout_for_window(&1);
    assert_eq!(
        after_layout, before_layout,
        "layout command should be a no-op on floating leaf",
    );

    check_ops_on_layout(&mut layout, [Op::SplitVertical]);

    let workspace = layout.active_workspace().expect("active workspace");
    assert_eq!(workspace.floating().tiles().count(), 1);
    assert_eq!(
        workspace.floating().root_layout_for_window(&1),
        Some(ContainerLayout::SplitV),
        "split on floating leaf should create an explicit SplitV wrapper (sway parity)",
    );
}
#[test]
fn i3_192_toggle_layout_all_cycles_floating_container_layouts() {
    let mut layout = check_ops([
        Op::AddOutput(1),
        Op::AddWindow {
            params: TestWindowParams::new(1),
        },
        Op::ToggleWindowFloating { id: None },
        Op::SplitVertical,
        Op::AddWindow {
            params: TestWindowParams::new(2),
        },
    ]);

    let workspace = layout.active_workspace().expect("active workspace");
    assert_eq!(
        workspace.floating().root_layout_for_window(&1),
        Some(ContainerLayout::SplitV),
    );

    check_ops_on_layout(&mut layout, [Op::ToggleLayoutAll]);
    let workspace = layout.active_workspace().expect("active workspace");
    assert_eq!(
        workspace.floating().root_layout_for_window(&1),
        Some(ContainerLayout::Stacked),
        "toggle_layout_all should cycle floating container layout from SplitV to Stacked",
    );

    check_ops_on_layout(&mut layout, [Op::ToggleLayoutAll]);
    let workspace = layout.active_workspace().expect("active workspace");
    assert_eq!(
        workspace.floating().root_layout_for_window(&1),
        Some(ContainerLayout::Tabbed),
        "toggle_layout_all should cycle floating container layout from Stacked to Tabbed",
    );

    check_ops_on_layout(&mut layout, [Op::ToggleLayoutAll]);
    let workspace = layout.active_workspace().expect("active workspace");
    assert_eq!(
        workspace.floating().root_layout_for_window(&1),
        Some(ContainerLayout::SplitH),
        "toggle_layout_all should cycle floating container layout from Tabbed to SplitH",
    );
}
#[test]
fn i3_218_toggle_layout_all_on_floating_leaf_is_noop() {
    let mut params = TestWindowParams::new(1);
    params.is_floating = true;

    let mut layout = check_ops([Op::AddOutput(1), Op::AddWindow { params }]);

    let before = layout
        .active_workspace()
        .expect("active workspace")
        .floating()
        .root_layout_for_window(&1);

    check_ops_on_layout(&mut layout, [Op::ToggleLayoutAll]);

    let after = layout
        .active_workspace()
        .expect("active workspace")
        .floating()
        .root_layout_for_window(&1);
    assert_eq!(
        after, before,
        "toggle_layout_all should be a no-op on a floating leaf without an explicit wrapper",
    );
}
#[test]
fn i3_192_set_layout_on_floating_container_with_children_retargets_wrapper() {
    let mut layout = check_ops([
        Op::AddOutput(1),
        Op::AddWindow {
            params: TestWindowParams::new(1),
        },
        Op::ToggleWindowFloating { id: None },
        Op::SplitVertical,
        Op::AddWindow {
            params: TestWindowParams::new(2),
        },
    ]);

    let workspace = layout.active_workspace().expect("active workspace");
    assert_eq!(
        workspace.floating().root_layout_for_window(&1),
        Some(ContainerLayout::SplitV),
    );

    check_ops_on_layout(&mut layout, [Op::SetLayoutTabbed]);
    let workspace = layout.active_workspace().expect("active workspace");
    assert_eq!(
        workspace.floating().root_layout_for_window(&1),
        Some(ContainerLayout::Tabbed),
        "set_layout should retarget the active floating container wrapper",
    );

    check_ops_on_layout(&mut layout, [Op::SetLayoutStacked]);
    let workspace = layout.active_workspace().expect("active workspace");
    assert_eq!(
        workspace.floating().root_layout_for_window(&1),
        Some(ContainerLayout::Stacked),
        "set_layout should continue to mutate the same floating container wrapper",
    );
}
#[test]
fn i3_510_cross_output_focus_uses_target_workspace_mru_leaf() {
    // Mirrors i3_test_cases/t/510-focus-across-outputs.t (#1160 section):
    // crossing outputs should focus the MRU leaf in target workspace, not the geometric first leaf.
    let layout = check_ops([
        Op::AddOutput(1),
        Op::AddOutput(2),
        Op::FocusOutput(2),
        Op::AddWindow {
            params: TestWindowParams::new(1),
        },
        Op::AddWindow {
            params: TestWindowParams::new(2),
        },
        Op::FocusColumnLeft,
        Op::FocusOutput(1),
        Op::AddWindow {
            params: TestWindowParams::new(3),
        },
        Op::FocusColumnOrMonitorRight(2),
    ]);

    let focused = layout
        .focus()
        .map(|win| *win.id())
        .expect("focused window should exist");
    assert_eq!(
        focused, 1,
        "cross-output focus should land on target workspace MRU leaf",
    );
}
#[test]
fn i3_510_cross_output_focus_prefers_tiling_over_destination_floating() {
    let layout = check_ops([
        Op::AddOutput(1),
        Op::AddOutput(2),
        Op::AddWindow {
            params: TestWindowParams::new(1),
        },
        Op::AddWindow {
            params: TestWindowParams::new(2),
        },
        Op::AddWindow {
            params: TestWindowParams::new(3),
        },
        Op::ToggleWindowFloating { id: Some(3) },
        Op::FocusWindow(2),
        Op::FocusFloating,
        Op::FocusOutput(2),
        Op::AddWindow {
            params: TestWindowParams::new(4),
        },
        Op::FocusColumnOrMonitorLeft(1),
    ]);

    let focused = layout
        .focus()
        .map(|win| *win.id())
        .expect("focused window should exist");
    assert_eq!(
        focused, 2,
        "cross-output focus should not land on destination floating when a tiling candidate exists",
    );

    let workspace = layout.active_workspace().expect("active workspace");
    assert!(
        !workspace.is_floating(&2),
        "cross-output focus should land on the destination tiling leaf, not the floating window",
    );
}
#[test]
fn i3_510_cross_output_focus_uses_focused_descendant_in_tabbed_target() {
    let layout = check_ops([
        Op::AddOutput(1),
        Op::AddOutput(2),
        Op::FocusOutput(2),
        Op::AddWindow {
            params: TestWindowParams::new(1),
        },
        Op::AddWindow {
            params: TestWindowParams::new(2),
        },
        Op::SetLayoutTabbed,
        Op::FocusWindow(1),
        Op::FocusOutput(1),
        Op::AddWindow {
            params: TestWindowParams::new(3),
        },
        Op::FocusColumnOrMonitorRight(2),
    ]);

    assert_eq!(
        layout.focus().map(|win| *win.id()),
        Some(1),
        "cross-output focus should land on the focused tab inside the destination tabbed container",
    );
}
#[test]
fn i3_510_cross_output_focus_uses_focused_descendant_in_stacked_target() {
    let layout = check_ops([
        Op::AddOutput(1),
        Op::AddOutput(2),
        Op::FocusOutput(2),
        Op::AddWindow {
            params: TestWindowParams::new(1),
        },
        Op::AddWindow {
            params: TestWindowParams::new(2),
        },
        Op::SetLayoutStacked,
        Op::FocusWindow(1),
        Op::FocusOutput(1),
        Op::AddWindow {
            params: TestWindowParams::new(3),
        },
        Op::FocusColumnOrMonitorRight(2),
    ]);

    assert_eq!(
        layout.focus().map(|win| *win.id()),
        Some(1),
        "cross-output focus should land on the focused child inside the destination stacked container",
    );
}
#[test]
fn i3_510_cross_output_focus_uses_nested_focused_descendant() {
    let layout = check_ops([
        Op::AddOutput(1),
        Op::AddOutput(2),
        Op::FocusOutput(2),
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
        Op::FocusWindow(3),
        Op::SplitHorizontal,
        Op::AddWindow {
            params: TestWindowParams::new(4),
        },
        Op::FocusWindow(4),
        Op::FocusOutput(1),
        Op::AddWindow {
            params: TestWindowParams::new(5),
        },
        Op::FocusColumnOrMonitorRight(2),
    ]);

    assert_eq!(
        layout.focus().map(|win| *win.id()),
        Some(4),
        "cross-output focus should descend into the focused nested leaf of the destination workspace",
    );
}
#[test]
fn i3_520_cross_output_focus_falls_back_to_existing_floating_window() {
    let layout = check_ops([
        Op::AddOutput(1),
        Op::AddOutput(2),
        Op::AddWindow {
            params: TestWindowParams::new(1),
        },
        Op::ToggleWindowFloating { id: None },
        Op::FocusOutput(2),
        Op::FocusColumnOrMonitorLeft(1),
    ]);

    let focused = layout
        .focus()
        .map(|win| *win.id())
        .expect("focused window should exist");
    assert_eq!(
        focused, 1,
        "cross-output focus should target the floating window when the destination output has no tiling candidate",
    );

    let workspace = layout.active_workspace().expect("active workspace");
    assert!(
        workspace.is_floating(&1),
        "cross-output directional focus should land on the floating container itself",
    );
}
#[test]
fn i3_550_repeated_splits_on_a_lone_window_build_no_wrapper() {
    // Measured against sway 1.11: `split v; split h; split v` on a lone window leaves the
    // tree flat and the workspace splitv, so the next window stacks under window 1. Each
    // command only restates the workspace orientation — the container the name of this test
    // used to claim was never sway's.
    let layout = check_ops([
        Op::AddOutput(1),
        Op::AddWindow {
            params: TestWindowParams::new(1),
        },
        Op::SplitVertical,
        Op::SplitHorizontal,
        Op::SplitVertical,
        Op::AddWindow {
            params: TestWindowParams::new(2),
        },
    ]);

    let workspace = layout.active_workspace().expect("active workspace");
    assert_eq!(workspace.debug_workspace_layout(), ContainerLayout::SplitV);
    let tree = workspace.container_tree().debug_tree();
    assert_snapshot!(
        tree.as_str(),
        @"
    SplitV
      Window 1
      Window 2 *
    "
    );
}
#[test]
fn i3_550_tabbed_then_stacked_on_single_leaf_keeps_single_wrapper() {
    let layout = check_ops([
        Op::AddOutput(1),
        Op::AddWindow {
            params: TestWindowParams::new(1),
        },
        Op::SetLayoutTabbed,
        Op::SetLayoutStacked,
    ]);

    let workspace = layout.active_workspace().expect("active workspace");
    let tree = workspace.container_tree().debug_tree();
    assert_snapshot!(
        tree.as_str(),
        @"
    SplitH
      Stacked
        Window 1 *
    "
    );
}
#[test]
fn i3_550_split_inside_tabbed_keeps_single_nested_split_wrapper() {
    let layout = check_ops([
        Op::AddOutput(1),
        Op::AddWindow {
            params: TestWindowParams::new(1),
        },
        Op::SetLayoutTabbed,
        Op::SplitVertical,
    ]);

    let workspace = layout.active_workspace().expect("active workspace");
    let tree = workspace.container_tree().debug_tree();
    assert_snapshot!(
        tree.as_str(),
        @"
    SplitH
      Tabbed
        SplitV
          Window 1 *
    "
    );
}
#[test]
fn i3_550_sway_112_flattens_the_lone_split_before_the_next_layout() {
    let layout = check_ops([
        Op::AddOutput(1),
        Op::AddWindow {
            params: TestWindowParams::new(1),
        },
        Op::SetLayoutTabbed,
        Op::SplitVertical,
        Op::SetLayoutTabbed,
        Op::SplitVertical,
    ]);

    let workspace = layout.active_workspace().expect("active workspace");
    let tree = workspace.container_tree().debug_tree();
    assert_snapshot!(
        tree.as_str(),
        @"
    SplitH
      Tabbed
        SplitV
          Window 1 *
    "
    );
}
#[test]
fn i3_550_tabbed_with_two_nodes_inside_other_tabbed_stays_two_level() {
    let layout = check_ops([
        Op::AddOutput(1),
        Op::AddWindow {
            params: TestWindowParams::new(1),
        },
        Op::SetLayoutTabbed,
        Op::SplitVertical,
        Op::AddWindow {
            params: TestWindowParams::new(2),
        },
        Op::SetLayoutTabbed,
    ]);

    let workspace = layout.active_workspace().expect("active workspace");
    let tree = workspace.container_tree().debug_tree();
    assert_snapshot!(
        tree.as_str(),
        @"
    SplitH
      Tabbed
        Tabbed
          Window 1
          Window 2 *
    "
    );
}
#[test]
fn i3_550_repeat_tabbed_layout_does_not_create_redundant_wrappers() {
    // Mirrors i3_test_cases/t/550-split-redundant-containers.t ("repeat tabbed layout").
    let layout = check_ops([
        Op::AddOutput(1),
        Op::AddWindow {
            params: TestWindowParams::new(1),
        },
        Op::SetLayoutTabbed,
        Op::SetLayoutTabbed,
        Op::SetLayoutTabbed,
        Op::SetLayoutTabbed,
        Op::SetLayoutTabbed,
    ]);

    let workspace = layout.active_workspace().expect("active workspace");
    let tree = workspace.container_tree().debug_tree();
    assert_snapshot!(
        tree.as_str(),
        @"
    SplitH
      Tabbed
        Window 1 *
    "
    );
}
#[test]
fn i3_550_layout_tabbed_flattens_the_lone_split_like_sway_112() {
    // Mirrors i3_test_cases/t/550-split-redundant-containers.t
    // ("split v inside tabbed and back to just tabbed").
    let layout = check_ops([
        Op::AddOutput(1),
        Op::AddWindow {
            params: TestWindowParams::new(1),
        },
        Op::SetLayoutTabbed,
        Op::SplitVertical,
        Op::SetLayoutTabbed,
    ]);

    let workspace = layout.active_workspace().expect("active workspace");
    let tree = workspace.container_tree().debug_tree();
    assert_snapshot!(
        tree.as_str(),
        @"
    SplitH
      Tabbed
        Window 1 *
    "
    );
}
/// Border colors that name the state they came from, so a failure reads as the state that
/// was painted rather than as four floats.
fn decoration_options() -> Options {
    let color = |r, g, b| tiri_config::Color::from_rgba8_unpremul(r, g, b, 255);
    Options {
        layout: tiri_config::Layout {
            border: tiri_config::Border {
                off: false,
                width: 2.,
                // The sway defaults, which is what this is about.
                active_color: color(0x28, 0x55, 0x77),
                focused_inactive_color: color(0x5f, 0x67, 0x6a),
                inactive_color: color(0x22, 0x22, 0x22),
                ..Default::default()
            },
            focus_ring: tiri_config::FocusRing {
                off: true,
                ..Default::default()
            },
            ..Default::default()
        },
        ..Default::default()
    }
}

#[derive(Debug, PartialEq, Eq)]
enum Decoration {
    Focused,
    FocusedInactive,
    Unfocused,
    Other,
}

fn decorations(layout: &Layout<TestWindow>) -> HashMap<usize, Decoration> {
    let borders = &layout.options.layout.border;
    let matches = |color: smithay::backend::renderer::Color32F, expected: tiri_config::Color| {
        let expected: [f32; 4] = expected.to_array_unpremul();
        (0..4).all(|i| (color.components()[i] - expected[i]).abs() < 1e-3)
    };

    let workspace = layout.active_workspace().expect("active workspace");
    workspace
        .tiles_with_render_positions()
        .map(|(tile, _, _)| {
            let color = tile.debug_border_color();
            let state = if matches(color, borders.active_color) {
                Decoration::Focused
            } else if matches(color, borders.focused_inactive_color) {
                Decoration::FocusedInactive
            } else if matches(color, borders.inactive_color) {
                Decoration::Unfocused
            } else {
                Decoration::Other
            };
            (*tile.window().id(), state)
        })
        .collect()
}

#[test]
fn focused_inactive_is_the_focus_head_of_a_non_focused_container() {
    // SplitH[ SplitV[win1, win3], win2 ] with the focus on win2. sway renders a level at a
    // time: at the root, win2 is the focus-inactive child and is also focused; inside the
    // SplitV nobody is focused, but the container still points at win3, so win3 is
    // `focused_inactive` and win1 — which its parent would not come back to — is not.
    let mut layout = check_ops_with_options(
        decoration_options(),
        [
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
            Op::FocusWindow(2),
        ],
    );
    layout.update_render_elements(None);

    let workspace = layout.active_workspace().expect("active workspace");
    assert_snapshot!(
        workspace.container_tree().debug_tree().as_str(),
        @"
    SplitH
      SplitV
        Window 1
        Window 3
      Window 2 *
    "
    );

    let decorations = decorations(&layout);
    assert_eq!(decorations[&2], Decoration::Focused);
    assert_eq!(decorations[&3], Decoration::FocusedInactive);
    assert_eq!(decorations[&1], Decoration::Unfocused);
}

#[test]
fn focusing_a_float_leaves_the_tiled_window_focused_inactive() {
    // The state the flat "everything on the active workspace" model could not express: with
    // the focus on a float, the workspace still points at the window that had it, and that
    // one window — not every tiled window — is `focused_inactive`.
    let mut layout = check_ops_with_options(
        decoration_options(),
        [
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
            Op::ToggleWindowFloating { id: Some(3) },
            Op::FocusWindow(3),
        ],
    );
    layout.update_render_elements(None);

    let decorations = decorations(&layout);
    // The float itself: sway's `render_floating_container` only ever paints a lone float
    // focused, urgent or unfocused.
    assert_eq!(decorations[&3], Decoration::Focused);
    // The tiled window the workspace would return to.
    assert_eq!(decorations[&2], Decoration::FocusedInactive);
    // Its sibling, which nothing points at.
    assert_eq!(decorations[&1], Decoration::Unfocused);
}

#[test]
fn a_lone_float_is_never_focused_inactive() {
    let mut layout = check_ops_with_options(
        decoration_options(),
        [
            Op::AddOutput(1),
            Op::AddWindow {
                params: TestWindowParams::new(1),
            },
            Op::AddWindow {
                params: TestWindowParams::new(2),
            },
            Op::ToggleWindowFloating { id: Some(2) },
            Op::FocusWindow(1),
        ],
    );
    layout.update_render_elements(None);

    let decorations = decorations(&layout);
    assert_eq!(decorations[&1], Decoration::Focused);
    // sway never compares a lone float against the workspace's focus-inactive child, so it
    // drops straight to unfocused however recently it was focused.
    assert_eq!(decorations[&2], Decoration::Unfocused);
}

/// `move position center` is a floating-layer command; a tiled window has no position.
///
/// sway refuses it outright: `cmd_move_to_position` fails with "Only floating containers can
/// be moved to an absolute position" (sway/commands/move.c:818). tiri accepts the
/// action and does nothing, because the same binding has to keep working when the focus
/// moves to a floating window. This pins the "nothing" half: without it, the inert branch in
/// `ContainerTree::center_window` looks like an unfinished stub and invites someone to invent a
/// meaning for centering a window inside a tree.
#[test]
fn centering_a_tiled_window_does_nothing() {
    let mut layout = check_ops([
        Op::AddOutput(1),
        Op::AddWindow {
            params: TestWindowParams::new(1),
        },
        Op::AddWindow {
            params: TestWindowParams::new(2),
        },
        Op::Communicate(1),
        Op::Communicate(2),
        Op::CompleteAnimations,
    ]);

    let before = [tile_rect(&layout, 1), tile_rect(&layout, 2)];

    check_ops_on_layout(
        &mut layout,
        [
            Op::CenterWindow { id: Some(1) },
            Op::CenterWindow { id: None },
            Op::Communicate(1),
            Op::Communicate(2),
            Op::CompleteAnimations,
        ],
    );

    let after = [tile_rect(&layout, 1), tile_rect(&layout, 2)];
    assert_eq!(
        before, after,
        "centering moved a tiled window; it has no position of its own to set"
    );
}
