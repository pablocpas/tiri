use insta::assert_snapshot;

use super::super::container::Layout as ContainerLayout;
use super::*;

#[test]
fn parity_seed2_step60_toggle_floating_restores_stacked_subtree_like_sway() {
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
    ]);

    check_ops_on_layout(
        &mut layout,
        [
            Op::FocusColumnRight,
            Op::FocusColumnRight,
            Op::FocusColumnRight,
            Op::SetLayoutTabbed,
            Op::FocusWindowDown,
            Op::SetLayoutSplitV,
            Op::SplitVertical,
            Op::AddWindow {
                params: TestWindowParams::new(5),
            },
            Op::SplitHorizontal,
            Op::AddWindow {
                params: TestWindowParams::new(6),
            },
            Op::FocusColumnLeft,
            Op::CloseFocused,
            Op::FocusWindowDown,
            Op::FocusParent,
            Op::AddWindow {
                params: TestWindowParams::new(7),
            },
            Op::FocusParent,
            Op::ToggleWindowFloating { id: None },
            Op::SetLayoutStacked,
            Op::SwitchFocusFloatingTiling,
            Op::FocusChild,
            Op::ToggleWindowFloating { id: None },
            Op::SetLayoutSplitH,
            Op::FocusColumnLeft,
            Op::FocusColumnLeft,
            Op::SetLayoutTabbed,
            Op::FocusChild,
            Op::ToggleSplitLayout,
            Op::SetLayoutStacked,
            Op::FocusParent,
            Op::SwitchFocusFloatingTiling,
            Op::FocusWindowDown,
            Op::ToggleFullscreenFocused,
            Op::FocusWindowDown,
            Op::SplitVertical,
            Op::SplitVertical,
            Op::FocusColumnLeft,
            Op::FocusWindowDown,
            Op::ToggleSplitLayout,
            Op::FocusWindowDown,
            Op::FocusWindowUp,
            Op::ToggleWindowFloating { id: None },
            Op::ToggleWindowFloating { id: None },
            Op::SetLayoutTabbed,
            Op::ToggleWindowFloating { id: None },
            Op::ToggleFullscreenFocused,
            Op::FocusWindowDown,
            Op::FocusChild,
            Op::FocusParent,
            Op::SwitchFocusFloatingTiling,
            Op::SetLayoutTabbed,
            Op::AddWindow {
                params: TestWindowParams::new(8),
            },
            Op::SetLayoutTabbed,
            Op::SetLayoutTabbed,
            Op::FocusChild,
            Op::FocusWindowDown,
            Op::FocusParent,
            Op::FocusChild,
            Op::SwitchFocusFloatingTiling,
            Op::SplitVertical,
        ],
    );

    check_ops_on_layout(&mut layout, [Op::ToggleWindowFloating { id: None }]);

    let ws = layout.active_workspace().expect("active workspace");
    let tree = ws.tiling().debug_tree().replace(" *", "");
    assert!(
        tree.starts_with("Tabbed\n  Window 8\n  Stacked\n    SplitV\n"),
        "step60 toggle_floating should restore the floating subtree under the tabbed workspace root like sway:\n{tree}"
    );
    assert!(
        tree.contains("Stacked\n    SplitV\n      Window 1")
            && tree.contains("    SplitV\n      Window 7"),
        "step60 toggle_floating should restore the stacked subtree with the splitv child holding window 7 like sway:\n{tree}"
    );
    assert_eq!(
        ws.tiling().focus_path(),
        vec![1, 1, 0],
        "step60 focus should land on the restored floating leaf like sway",
    );
}
#[test]
fn parity_seed1_focus_parent_on_single_child_floating_wrapper_keeps_floating_mode() {
    let mut layout = Layout::default();
    check_ops_on_layout(
        &mut layout,
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
            Op::AddWindow {
                params: TestWindowParams::new(4),
            },
        ],
    );

    let ops = [
        "focus_up",
        "close_focused",
        "focus_right",
        "split_v",
        "focus_up",
        "toggle_floating",
        "focus_child",
        "focus_child",
        "layout_stacked",
        "split_h",
        "focus_up",
        "toggle_floating",
        "focus_left",
        "layout_stacked",
        "focus_parent",
        "open_window",
        "focus_left",
        "focus_child",
        "layout_splith",
        "split_v",
        "open_window",
        "focus_up",
        "layout_toggle_split",
        "focus_left",
        "focus_left",
        "focus_left",
        "toggle_focus_mode",
        "focus_left",
        "layout_stacked",
        "split_h",
        "focus_parent",
        "focus_left",
        "toggle_focus_mode",
        "split_h",
        "focus_child",
        "toggle_floating",
        "toggle_fullscreen",
        "split_v",
        "layout_tabbed",
        "split_v",
        "split_h",
        "focus_child",
        "layout_splitv",
        "focus_left",
        "focus_parent",
        "toggle_fullscreen",
        "open_window",
        "focus_up",
        "focus_down",
        "open_window",
        "layout_splitv",
        "focus_up",
        "layout_toggle_split",
        "toggle_floating",
        "focus_parent",
        "toggle_floating",
        "split_h",
        "layout_splitv",
        "layout_splith",
        "open_window",
        "toggle_floating",
        "toggle_floating",
    ];

    let mut next_id = 5usize;
    for op in ops {
        match op {
            "focus_left" => layout.focus_left(),
            "focus_right" => layout.focus_right(),
            "focus_up" => layout.focus_up(),
            "focus_down" => layout.focus_down(),
            "split_h" => layout.split_horizontal(),
            "split_v" => layout.split_vertical(),
            "layout_splith" => layout.set_layout_mode(ContainerLayout::SplitH),
            "layout_splitv" => layout.set_layout_mode(ContainerLayout::SplitV),
            "layout_toggle_split" => layout.toggle_split_layout(),
            "layout_tabbed" => layout.set_layout_mode(ContainerLayout::Tabbed),
            "layout_stacked" => layout.set_layout_mode(ContainerLayout::Stacked),
            "focus_parent" => layout.focus_parent(),
            "focus_child" => layout.focus_child(),
            "toggle_floating" => layout.toggle_window_floating(None),
            "toggle_focus_mode" => layout.switch_focus_floating_tiling(),
            "toggle_fullscreen" => {
                if let Some(id) = layout.focus().map(|win| *win.id()) {
                    layout.toggle_fullscreen(&id);
                }
            }
            "close_focused" => {
                if let Some(id) = layout.focus().map(|win| *win.id()) {
                    layout.remove_window(&id, Transaction::new());
                }
            }
            "open_window" => {
                layout.add_window(
                    TestWindow::new(TestWindowParams::new(next_id)),
                    AddWindowTarget::Auto,
                    None,
                    None,
                    false,
                    false,
                    ActivateWindow::default(),
                );
                next_id += 1;
            }
            _ => panic!("unsupported op in replay: {op}"),
        }
    }

    let workspace = layout.active_workspace().expect("active workspace");
    let focus_id = layout.focus().map(|w| *w.id());
    assert!(workspace.floating_is_active());
    if let Some(id) = focus_id {
        assert!(!workspace.floating().selected_is_container(Some(&id)));
        assert!(!workspace.floating().wrapper_selected_for_window(&id));
    }

    layout.focus_parent();

    let workspace = layout.active_workspace().expect("active workspace");
    assert!(
        workspace.floating_is_active(),
        "focus_parent on this redundant single-child floating wrapper should keep floating mode (sway parity)",
    );
    let focus_id = layout.focus().map(|w| *w.id()).expect("focused window");
    assert!(!workspace.floating().selected_is_container(Some(&focus_id)));
    assert!(!workspace.floating().wrapper_selected_for_window(&focus_id));

    layout.add_window(
        TestWindow::new(TestWindowParams::new(next_id)),
        AddWindowTarget::Auto,
        None,
        None,
        false,
        false,
        ActivateWindow::Yes,
    );

    let workspace_after_open = layout.active_workspace().expect("active workspace");
    assert!(
        workspace_after_open.floating_is_active(),
        "open_window after focus_parent in this scenario should keep floating mode (sway parity)",
    );
    let focus_id_after_open = layout
        .focus()
        .map(|w| *w.id())
        .expect("focused window after open");
    assert_eq!(
        focus_id_after_open, focus_id,
        "open_window should not steal focus from active floating window in this scenario"
    );
}
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
    let tree = workspace.tiling().debug_tree();
    assert!(
        workspace.tiling().contains_layout(ContainerLayout::Tabbed),
        "workspace layout tabbed should group the second open into a tabbed container:\n{tree}",
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
    let tree = workspace.tiling().debug_tree();
    assert!(
        workspace.tiling().contains_layout(ContainerLayout::Stacked),
        "workspace layout stacked should group the second open into a stacked container:\n{tree}",
    );
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
        false,
        ActivateWindow::default(),
    );

    let workspace = layout.active_workspace().expect("active workspace");
    let tree = workspace.tiling().debug_tree();
    assert!(
        workspace.tiling().contains_layout(ContainerLayout::Stacked),
        "workspace layout stacked should still apply after floating roundtrip reinsertion:\n{tree}",
    );
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
    layout.remove_window(&1, Transaction::new());
    layout.remove_window(&2, Transaction::new());

    layout.set_layout_mode(ContainerLayout::SplitH);
    layout.add_window(
        TestWindow::new(TestWindowParams::new(3)),
        AddWindowTarget::Auto,
        None,
        None,
        false,
        false,
        ActivateWindow::default(),
    );
    layout.add_window(
        TestWindow::new(TestWindowParams::new(4)),
        AddWindowTarget::Auto,
        None,
        None,
        false,
        false,
        ActivateWindow::default(),
    );

    let workspace = layout.active_workspace().expect("active workspace");
    let tree = workspace.tiling().debug_tree();
    assert!(
        !workspace.tiling().contains_layout(ContainerLayout::Stacked),
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
    layout.remove_window(&1, Transaction::new());
    layout.remove_window(&2, Transaction::new());

    layout.set_layout_mode(ContainerLayout::SplitV);
    layout.add_window(
        TestWindow::new(TestWindowParams::new(3)),
        AddWindowTarget::Auto,
        None,
        None,
        false,
        false,
        ActivateWindow::default(),
    );
    layout.add_window(
        TestWindow::new(TestWindowParams::new(4)),
        AddWindowTarget::Auto,
        None,
        None,
        false,
        false,
        ActivateWindow::default(),
    );

    let workspace = layout.active_workspace().expect("active workspace");
    let tree = workspace.tiling().debug_tree();
    assert!(
        workspace.tiling().contains_layout(ContainerLayout::SplitV),
        "after resetting empty workspace layout to splitv, new opens should land in a vertical split:\n{tree}",
    );
    assert!(
        !workspace.tiling().contains_layout(ContainerLayout::Tabbed),
        "after resetting empty workspace layout to splitv, new opens should no longer land in tabbed:\n{tree}",
    );
}
#[test]
fn parity_seed2_toggle_fullscreen_keeps_tiling_container_selection() {
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
    ]);

    check_ops_on_layout(
        &mut layout,
        [
            Op::FocusColumnRight,
            Op::FocusColumnRight,
            Op::FocusColumnRight,
            Op::SetLayoutTabbed,
            Op::FocusWindowDown,
            Op::SetLayoutSplitV,
            Op::SplitVertical,
            Op::AddWindow {
                params: TestWindowParams::new(5),
            },
            Op::SplitHorizontal,
            Op::AddWindow {
                params: TestWindowParams::new(6),
            },
            Op::FocusColumnLeft,
            Op::CloseFocused,
            Op::FocusWindowDown,
            Op::FocusParent,
            Op::AddWindow {
                params: TestWindowParams::new(7),
            },
            Op::FocusParent,
            Op::ToggleWindowFloating { id: None },
            Op::SetLayoutStacked,
            Op::SwitchFocusFloatingTiling,
            Op::FocusChild,
            Op::ToggleWindowFloating { id: None },
            Op::SetLayoutSplitH,
            Op::FocusColumnLeft,
            Op::FocusColumnLeft,
            Op::SetLayoutTabbed,
            Op::FocusChild,
            Op::ToggleSplitLayout,
            Op::SetLayoutStacked,
            Op::FocusParent,
            Op::SwitchFocusFloatingTiling,
            Op::FocusWindowDown,
        ],
    );

    {
        let workspace = layout.active_workspace().expect("active workspace");
        assert!(
            workspace.tiling().selected_is_container(),
            "replay precondition: focus-parent selection must be active before toggle_fullscreen",
        );
    }

    check_ops_on_layout(&mut layout, [Op::ToggleFullscreenFocused]);

    let workspace = layout.active_workspace().expect("active workspace");
    assert!(
        workspace.tiling().selected_is_container(),
        "toggle_fullscreen should not clear the active tiling container selection in this sway parity path",
    );
}
#[test]
fn parity_seed2_step42_toggle_floating_restores_workspace_subtree_to_tiling() {
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
    ]);

    check_ops_on_layout(
        &mut layout,
        [
            Op::FocusColumnRight,
            Op::FocusColumnRight,
            Op::FocusColumnRight,
            Op::SetLayoutTabbed,
            Op::FocusWindowDown,
            Op::SetLayoutSplitV,
            Op::SplitVertical,
            Op::AddWindow {
                params: TestWindowParams::new(5),
            },
            Op::SplitHorizontal,
            Op::AddWindow {
                params: TestWindowParams::new(6),
            },
            Op::FocusColumnLeft,
            Op::CloseFocused,
            Op::FocusWindowDown,
            Op::FocusParent,
            Op::AddWindow {
                params: TestWindowParams::new(7),
            },
            Op::FocusParent,
            Op::ToggleWindowFloating { id: None },
            Op::SetLayoutStacked,
            Op::SwitchFocusFloatingTiling,
            Op::FocusChild,
            Op::ToggleWindowFloating { id: None },
            Op::SetLayoutSplitH,
            Op::FocusColumnLeft,
            Op::FocusColumnLeft,
            Op::SetLayoutTabbed,
            Op::FocusChild,
            Op::ToggleSplitLayout,
            Op::SetLayoutStacked,
            Op::FocusParent,
            Op::SwitchFocusFloatingTiling,
            Op::FocusWindowDown,
            Op::ToggleFullscreenFocused,
            Op::FocusWindowDown,
            Op::SplitVertical,
            Op::SplitVertical,
            Op::FocusColumnLeft,
            Op::FocusWindowDown,
            Op::ToggleSplitLayout,
            Op::FocusWindowDown,
            Op::FocusWindowUp,
            Op::ToggleWindowFloating { id: None },
            Op::ToggleWindowFloating { id: None },
        ],
    );

    let workspace = layout.active_workspace().expect("active workspace");
    assert!(
        !workspace.floating_is_active(),
        "step 42 second toggle_floating should restore focus mode to tiling (sway parity)",
    );
    assert_eq!(
        workspace.floating().tiles().count(),
        0,
        "step 42 second toggle_floating should empty floating workspace subtree",
    );
    assert_eq!(
        workspace.tiling().tiles().count(),
        6,
        "step 42 second toggle_floating should restore all windows to tiling",
    );
}
#[test]
fn parity_seed2_step42_unfloat_from_floating_workspace_context_preserves_workspace_context() {
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
    ]);

    check_ops_on_layout(
        &mut layout,
        [
            Op::FocusColumnRight,
            Op::FocusColumnRight,
            Op::FocusColumnRight,
            Op::SetLayoutTabbed,
            Op::FocusWindowDown,
            Op::SetLayoutSplitV,
            Op::SplitVertical,
            Op::AddWindow {
                params: TestWindowParams::new(5),
            },
            Op::SplitHorizontal,
            Op::AddWindow {
                params: TestWindowParams::new(6),
            },
            Op::FocusColumnLeft,
            Op::CloseFocused,
            Op::FocusWindowDown,
            Op::FocusParent,
            Op::AddWindow {
                params: TestWindowParams::new(7),
            },
            Op::FocusParent,
            Op::ToggleWindowFloating { id: None },
            Op::SetLayoutStacked,
            Op::SwitchFocusFloatingTiling,
            Op::FocusChild,
            Op::ToggleWindowFloating { id: None },
            Op::SetLayoutSplitH,
            Op::FocusColumnLeft,
            Op::FocusColumnLeft,
            Op::SetLayoutTabbed,
            Op::FocusChild,
            Op::ToggleSplitLayout,
            Op::SetLayoutStacked,
            Op::FocusParent,
            Op::SwitchFocusFloatingTiling,
            Op::FocusWindowDown,
            Op::ToggleFullscreenFocused,
            Op::FocusWindowDown,
            Op::SplitVertical,
            Op::SplitVertical,
            Op::FocusColumnLeft,
            Op::FocusWindowDown,
            Op::ToggleSplitLayout,
            Op::FocusWindowDown,
            Op::FocusWindowUp,
            Op::ToggleWindowFloating { id: None },
            Op::ToggleWindowFloating { id: None },
        ],
    );

    let workspace = layout.active_workspace().expect("active workspace");
    assert_eq!(
        workspace.debug_command_context(),
        "workspace",
        "restoring from floating workspace context should keep workspace command context like sway",
    );
}
#[test]
fn parity_seed2_step43_layout_tabbed_wraps_workspace_subtree_like_sway() {
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
    ]);

    check_ops_on_layout(
        &mut layout,
        [
            Op::FocusColumnRight,
            Op::FocusColumnRight,
            Op::FocusColumnRight,
            Op::SetLayoutTabbed,
            Op::FocusWindowDown,
            Op::SetLayoutSplitV,
            Op::SplitVertical,
            Op::AddWindow {
                params: TestWindowParams::new(5),
            },
            Op::SplitHorizontal,
            Op::AddWindow {
                params: TestWindowParams::new(6),
            },
            Op::FocusColumnLeft,
            Op::CloseFocused,
            Op::FocusWindowDown,
            Op::FocusParent,
            Op::AddWindow {
                params: TestWindowParams::new(7),
            },
            Op::FocusParent,
            Op::ToggleWindowFloating { id: None },
            Op::SetLayoutStacked,
            Op::SwitchFocusFloatingTiling,
            Op::FocusChild,
            Op::ToggleWindowFloating { id: None },
            Op::SetLayoutSplitH,
            Op::FocusColumnLeft,
            Op::FocusColumnLeft,
            Op::SetLayoutTabbed,
            Op::FocusChild,
            Op::ToggleSplitLayout,
            Op::SetLayoutStacked,
            Op::FocusParent,
            Op::SwitchFocusFloatingTiling,
            Op::FocusWindowDown,
            Op::ToggleFullscreenFocused,
            Op::FocusWindowDown,
            Op::SplitVertical,
            Op::SplitVertical,
            Op::FocusColumnLeft,
            Op::FocusWindowDown,
            Op::ToggleSplitLayout,
            Op::FocusWindowDown,
            Op::FocusWindowUp,
            Op::ToggleWindowFloating { id: None },
            Op::ToggleWindowFloating { id: None },
        ],
    );

    {
        let workspace = layout.active_workspace().expect("active workspace");
        assert_eq!(
            workspace.debug_command_context(),
            "workspace",
            "step 42 should leave layout commands targeting the workspace like sway",
        );
    }

    layout.set_layout_mode(ContainerLayout::Tabbed);

    let workspace = layout.active_workspace().expect("active workspace");
    let tree = workspace.tiling().debug_tree().replace(" *", "");
    assert!(
        tree.starts_with("Tabbed\n  Stacked\n"),
        "workspace-context layout_tabbed should wrap the restored tiling subtree like sway:\n{tree}"
    );
    assert_eq!(
        workspace.tiling().focus_path(),
        vec![0, 1],
        "focus should remain on the same leaf inside the wrapped workspace subtree",
    );
}
#[test]
fn parity_seed2_step50_open_window_targets_tiling_from_floating_workspace_context_like_sway() {
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
    ]);

    check_ops_on_layout(
        &mut layout,
        [
            Op::FocusColumnRight,
            Op::FocusColumnRight,
            Op::FocusColumnRight,
            Op::SetLayoutTabbed,
            Op::FocusWindowDown,
            Op::SetLayoutSplitV,
            Op::SplitVertical,
            Op::AddWindow {
                params: TestWindowParams::new(5),
            },
            Op::SplitHorizontal,
            Op::AddWindow {
                params: TestWindowParams::new(6),
            },
            Op::FocusColumnLeft,
            Op::CloseFocused,
            Op::FocusWindowDown,
            Op::FocusParent,
            Op::AddWindow {
                params: TestWindowParams::new(7),
            },
            Op::FocusParent,
            Op::ToggleWindowFloating { id: None },
            Op::SetLayoutStacked,
            Op::SwitchFocusFloatingTiling,
            Op::FocusChild,
            Op::ToggleWindowFloating { id: None },
            Op::SetLayoutSplitH,
            Op::FocusColumnLeft,
            Op::FocusColumnLeft,
            Op::SetLayoutTabbed,
            Op::FocusChild,
            Op::ToggleSplitLayout,
            Op::SetLayoutStacked,
            Op::FocusParent,
            Op::SwitchFocusFloatingTiling,
            Op::FocusWindowDown,
            Op::ToggleFullscreenFocused,
            Op::FocusWindowDown,
            Op::SplitVertical,
            Op::SplitVertical,
            Op::FocusColumnLeft,
            Op::FocusWindowDown,
            Op::ToggleSplitLayout,
            Op::FocusWindowDown,
            Op::FocusWindowUp,
            Op::ToggleWindowFloating { id: None },
            Op::ToggleWindowFloating { id: None },
            Op::SetLayoutTabbed,
            Op::ToggleWindowFloating { id: None },
            Op::ToggleFullscreenFocused,
            Op::FocusWindowDown,
            Op::FocusChild,
            Op::FocusParent,
            Op::SwitchFocusFloatingTiling,
            Op::SetLayoutTabbed,
        ],
    );

    {
        let workspace = layout.active_workspace().expect("active workspace");
        assert!(
            workspace.floating_is_active(),
            "precondition: step 49 should still have floating active",
        );
        assert_eq!(workspace.tiling().tiles().count(), 0);
        assert_eq!(workspace.floating().tiles().count(), 6);
        assert_eq!(
            workspace.debug_command_context(),
            "floating",
            "precondition: step 49 should target a floating container path, not workspace",
        );
    }

    check_ops_on_layout(
        &mut layout,
        [Op::AddWindow {
            params: TestWindowParams::new(8),
        }],
    );

    let workspace = layout.active_workspace().expect("active workspace");
    assert_eq!(
        workspace.tiling().tiles().count(),
        1,
        "open_window from floating workspace context should create tiling like sway",
    );
    assert_eq!(
        workspace.floating().tiles().count(),
        6,
        "open_window from floating workspace context should not join floating subtree",
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
    let tree = workspace.tiling().debug_tree();
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
    let tree = workspace.tiling().debug_tree();
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
    let tree = workspace.tiling().debug_tree();
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
    let tree = workspace.tiling().debug_tree();
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
    let tree = workspace.tiling().debug_tree();
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
    let tree = workspace.tiling().debug_tree();
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
    let tree = workspace.tiling().debug_tree();
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
    let tree = workspace.tiling().debug_tree();
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
    let before = workspace.tiling().debug_tree().replace(" *", "");
    check_ops_on_layout(&mut layout, [Op::SplitVertical]);
    let workspace = layout.active_workspace().expect("active workspace");
    let after = workspace.tiling().debug_tree().replace(" *", "");
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
    let tree = workspace.tiling().debug_tree();
    assert_snapshot!(
        tree.as_str(),
        @"
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
    let tree = workspace.tiling().debug_tree();
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
    let tree = workspace.tiling().debug_tree();
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
fn i3_122_repeated_split_without_new_window_keeps_tree_shape() {
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
    let workspace = layout.active_workspace().expect("active workspace");
    let before = workspace.tiling().debug_tree().replace(" *", "");
    check_ops_on_layout(&mut layout, [Op::SplitVertical]);
    let workspace = layout.active_workspace().expect("active workspace");
    let after = workspace.tiling().debug_tree().replace(" *", "");
    assert_eq!(
        after, before,
        "repeating split without opening a new window should not create extra container structure",
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
        assert_snapshot!(workspace.tiling().debug_tree().as_str(), @"Window 1 *");
    }

    check_ops_on_layout(
        &mut layout,
        [Op::AddWindow {
            params: TestWindowParams::new(2),
        }],
    );

    let workspace = layout.active_workspace().expect("active workspace");
    assert_snapshot!(
        workspace.tiling().debug_tree().as_str(),
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
        workspace.tiling().debug_tree().as_str(),
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
    let before = workspace.tiling().debug_tree();
    check_ops_on_layout(
        &mut layout,
        [
            Op::MoveColumnLeft,
            Op::MoveColumnRight,
            Op::MoveWindowUp,
            Op::MoveWindowDown,
        ],
    );
    let workspace = layout.active_workspace().expect("active workspace");
    let after = workspace.tiling().debug_tree();
    assert_eq!(
        after, before,
        "moving a single container in any direction should be a no-op",
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
    let tree = workspace.tiling().debug_tree();
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
    let tree = workspace.tiling().debug_tree();
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
    let tree = workspace.tiling().debug_tree();
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
    let tree = workspace.tiling().debug_tree();
    assert_snapshot!(
        tree.as_str(),
        @"
    SplitH
      Window 1
      Window 2
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
    let before = workspace.tiling().debug_tree();
    assert_eq!(
        workspace.tiling().root_children_len(),
        3,
        "precondition: first phase of i3 145 ticket #1053 should still have 3 root children:\n{before}",
    );
    check_ops_on_layout(&mut layout, [Op::FocusColumnRight, Op::MoveColumnLeft]);
    let workspace = layout.active_workspace().expect("active workspace");
    let after = workspace.tiling().debug_tree();
    assert_eq!(
        workspace.tiling().root_children_len(),
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
    assert_eq!(workspace.tiling().tiles().count(), 0);
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
    assert_eq!(workspace.tiling().tiles().count(), 0);
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
    let tree = workspace.tiling().debug_tree();
    assert_snapshot!(
        tree.as_str(),
        @"
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

    let tree = workspace.tiling().debug_tree();
    assert_snapshot!(
        tree.as_str(),
        @"
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
    let after_left = workspace.tiling().debug_tree();
    assert_eq!(
        workspace.tiling().all_window_ids(),
        vec![2, 1],
        "moving the second root sibling left should swap it before the first:\n{after_left}",
    );
    assert_eq!(
        workspace.tiling().focused_window_id(),
        Some(2),
        "moving the second root sibling left should swap it before the first:\n{after_left}",
    );
    check_ops_on_layout(&mut layout, [Op::MoveColumnLeft]);
    let workspace = layout.active_workspace().expect("active workspace");
    let after_second_left = workspace.tiling().debug_tree();
    assert_eq!(
        workspace.tiling().all_window_ids(),
        vec![2, 1],
        "moving left again at the edge should be a no-op:\n{after_second_left}",
    );
    assert_eq!(
        workspace.tiling().focused_window_id(),
        Some(2),
        "moving left again at the edge should be a no-op:\n{after_second_left}",
    );
    check_ops_on_layout(&mut layout, [Op::MoveColumnRight]);
    let workspace = layout.active_workspace().expect("active workspace");
    let after_right = workspace.tiling().debug_tree();
    assert_eq!(
        workspace.tiling().all_window_ids(),
        vec![1, 2],
        "moving right should swap the root siblings back:\n{after_right}",
    );
    assert_eq!(
        workspace.tiling().focused_window_id(),
        Some(2),
        "moving right should swap the root siblings back:\n{after_right}",
    );
    check_ops_on_layout(&mut layout, [Op::MoveColumnRight]);
    let workspace = layout.active_workspace().expect("active workspace");
    let after_second_right = workspace.tiling().debug_tree();
    assert_eq!(
        workspace.tiling().all_window_ids(),
        vec![1, 2],
        "moving right again at the edge should be a no-op:\n{after_second_right}",
    );
    assert_eq!(
        workspace.tiling().focused_window_id(),
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
    let tree = workspace.tiling().debug_tree();
    let mut ids = workspace.tiling().all_window_ids();
    ids.sort_unstable();

    assert_eq!(
        workspace.tiling().root_children_len(),
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
    assert_eq!(workspace.tiling().tiles().count(), 1);
    assert_snapshot!(
        workspace.tiling().debug_tree().as_str(),
        @"
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
    let tree = workspace.tiling().debug_tree();
    assert!(
        workspace.tiling().focused_window_id() == Some(3),
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
        .tiling()
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
        let tree = workspace.tiling().debug_tree().replace(" *", "");
        assert!(
            workspace.tiling().windows().any(|win| *win.id() == 5),
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
        let tree = workspace.tiling().debug_tree();
        assert!(
            workspace.tiling().windows().any(|win| *win.id() == 4) && workspace.tiling().focused_window_id() == Some(5),
            "after the roundtrip both deep siblings should exist and window 5 should still be focused:\n{tree}",
        );
    }

    check_ops_on_layout(&mut layout, [Op::CloseWindow(5)]);
    assert_eq!(
        layout.focus().map(|win| *win.id()),
        Some(4),
        "after killing the focused deep sibling, focus should fall back to the restored floating-roundtrip window",
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
    assert_eq!(workspace.tiling().tiles().count(), 4);
    assert_snapshot!(
        workspace.tiling().debug_tree().as_str(),
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
        assert_eq!(workspace.tiling().tiles().count(), 0);
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
    assert_eq!(workspace.tiling().tiles().count(), 0);
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
fn i3_550_repeated_split_toggles_on_single_leaf_keep_one_wrapper() {
    let layout = check_ops([
        Op::AddOutput(1),
        Op::AddWindow {
            params: TestWindowParams::new(1),
        },
        Op::SplitVertical,
        Op::SplitHorizontal,
        Op::SplitVertical,
    ]);

    let workspace = layout.active_workspace().expect("active workspace");
    let tree = workspace.tiling().debug_tree();
    assert_snapshot!(
        tree.as_str(),
        @"
    SplitV
      Window 1 *
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
    let tree = workspace.tiling().debug_tree();
    assert_snapshot!(
        tree.as_str(),
        @"
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
    let tree = workspace.tiling().debug_tree();
    assert_snapshot!(
        tree.as_str(),
        @"
    Tabbed
      SplitV
        Window 1 *
    "
    );
}
#[test]
fn i3_550_toggle_split_inside_tabbed_does_not_create_redundant_wrappers() {
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
    let tree = workspace.tiling().debug_tree();
    assert_snapshot!(
        tree.as_str(),
        @"
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
    let tree = workspace.tiling().debug_tree();
    assert_snapshot!(
        tree.as_str(),
        @"
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
    let tree = workspace.tiling().debug_tree();
    assert_snapshot!(
        tree.as_str(),
        @"
    Tabbed
      Window 1 *
    "
    );
}
#[test]
fn i3_550_split_inside_tabbed_then_back_to_tabbed_flattens_split_wrapper() {
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
    let tree = workspace.tiling().debug_tree();
    assert_snapshot!(
        tree.as_str(),
        @"
    Tabbed
      Window 1 *
    "
    );
}
