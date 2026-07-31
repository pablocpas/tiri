use super::*;

#[test]
fn auto_add_window_does_not_inherit_floating_from_focused_window() {
    let layout = check_ops([
        Op::AddOutput(1),
        Op::AddWindow {
            params: TestWindowParams::new(1),
        },
        Op::SetWindowFloating {
            id: Some(1),
            floating: true,
        },
        Op::FocusFloating,
        Op::AddWindow {
            params: TestWindowParams::new(2),
        },
    ]);

    let workspace = layout.active_workspace().expect("active workspace");
    assert!(!workspace.is_floating(&2));
    assert!(window_layout(&layout, 2).pos_in_tiling_layout.is_some());
}

#[test]
fn opening_floating_window_clears_stale_tiling_workspace_context() {
    check_ops([
        Op::AddOutput(1),
        Op::AddWindow {
            params: TestWindowParams::new(1),
        },
        Op::FocusParent,
        Op::MoveWindowUpOrToWorkspaceUp,
        Op::MoveWorkspaceDown,
        Op::AddWindow {
            params: TestWindowParams {
                is_floating: true,
                ..TestWindowParams::new(2)
            },
        },
        Op::AddOutput(1),
    ]);
}

#[test]
fn opening_tiling_next_to_floating_clears_stale_floating_workspace_context() {
    check_ops([
        Op::AddOutput(1),
        Op::AddWindow {
            params: TestWindowParams::new(2),
        },
        Op::ToggleWindowFloating { id: None },
        Op::FocusParent,
        Op::SwapWindowInDirection(Direction::Left),
        Op::AddWindowNextTo {
            params: TestWindowParams::new(1),
            next_to_id: 2,
        },
    ]);
}

#[test]
fn focusing_output_from_floating_workspace_context_clears_tiling_workspace_context() {
    check_ops([
        Op::AddWindow {
            params: TestWindowParams {
                is_floating: true,
                ..TestWindowParams::new(2)
            },
        },
        Op::AddWindow {
            params: TestWindowParams {
                is_floating: true,
                ..TestWindowParams::new(1)
            },
        },
        Op::AddScaledOutput {
            id: 1,
            scale: 1.,
            layout_config: None,
        },
        Op::FocusWindowOrMonitorUp(1),
        Op::MaximizeWindowToEdges { id: None },
    ]);
}

#[test]
fn toggling_workspace_root_to_floating_clears_empty_tiling_selection() {
    check_ops([
        Op::AddWindow {
            params: TestWindowParams::new(1),
        },
        Op::AddOutput(1),
        Op::FocusParent,
        Op::SetLayoutTabbed,
        Op::SplitHorizontal,
        Op::ToggleWindowFloating { id: None },
    ]);
}

#[test]
fn scratchpad_show_clears_stale_tiling_workspace_context() {
    check_ops([
        Op::AddOutput(1),
        Op::AddWindow {
            params: TestWindowParams::new(4),
        },
        Op::MoveWindowToScratchpad { id: None },
        Op::AddWindow {
            params: TestWindowParams::new(1),
        },
        Op::FocusParent,
        Op::ScratchpadShow,
    ]);
}

#[test]
fn add_window_next_to_floating_does_not_inherit_floating() {
    let layout = check_ops([
        Op::AddOutput(1),
        Op::AddWindow {
            params: TestWindowParams::new(1),
        },
        Op::SetWindowFloating {
            id: Some(1),
            floating: true,
        },
        Op::AddWindowNextTo {
            params: TestWindowParams::new(2),
            next_to_id: 1,
        },
    ]);

    let workspace = layout.active_workspace().expect("active workspace");
    assert!(!workspace.is_floating(&2));
    assert!(window_layout(&layout, 2).pos_in_tiling_layout.is_some());
}
#[test]
fn add_window_next_to_floating_keeps_explicit_floating() {
    let mut params = TestWindowParams::new(2);
    params.is_floating = true;

    let layout = check_ops([
        Op::AddOutput(1),
        Op::AddWindow {
            params: TestWindowParams::new(1),
        },
        Op::SetWindowFloating {
            id: Some(1),
            floating: true,
        },
        Op::AddWindowNextTo {
            params,
            next_to_id: 1,
        },
    ]);

    let workspace = layout.active_workspace().expect("active workspace");
    assert!(workspace.is_floating(&2));
}
#[test]
fn auto_add_window_inherits_grouped_floating_after_split() {
    let layout = check_ops([
        Op::AddOutput(1),
        Op::AddWindow {
            params: TestWindowParams::new(1),
        },
        Op::SetWindowFloating {
            id: Some(1),
            floating: true,
        },
        Op::FocusFloating,
        Op::SplitVertical,
        Op::AddWindow {
            params: TestWindowParams::new(2),
        },
    ]);

    let workspace = layout.active_workspace().expect("active workspace");
    assert!(workspace.is_floating(&1));
    assert!(workspace.is_floating(&2));
    assert_eq!(
        workspace.floating().root_layout_for_window(&1),
        Some(ContainerLayout::SplitV)
    );
    assert_eq!(
        workspace.floating().root_layout_for_window(&2),
        Some(ContainerLayout::SplitV)
    );
}
#[test]
fn add_window_next_to_grouped_floating_inherits_group() {
    let layout = check_ops([
        Op::AddOutput(1),
        Op::AddWindow {
            params: TestWindowParams::new(1),
        },
        Op::SetWindowFloating {
            id: Some(1),
            floating: true,
        },
        Op::FocusFloating,
        Op::SplitVertical,
        Op::AddWindowNextTo {
            params: TestWindowParams::new(2),
            next_to_id: 1,
        },
    ]);

    let workspace = layout.active_workspace().expect("active workspace");
    assert!(workspace.is_floating(&1));
    assert!(workspace.is_floating(&2));
    assert_eq!(
        workspace.floating().root_layout_for_window(&1),
        Some(ContainerLayout::SplitV)
    );
    assert_eq!(
        workspace.floating().root_layout_for_window(&2),
        Some(ContainerLayout::SplitV)
    );
}
#[test]
fn open_window_joins_grouped_floating_even_when_tiling_is_empty() {
    // Sway parity: in floating mode with an explicitly split floating container,
    // opening a regular window should join that floating container even if tiling is empty.
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

    {
        let workspace = layout.active_workspace().expect("active workspace");
        assert!(workspace.floating_is_active());
        assert_eq!(workspace.tiling().tiles().count(), 0);
        assert_eq!(workspace.floating().tiles().count(), 2);
        assert_eq!(
            workspace.floating().root_layout_for_window(&1),
            Some(ContainerLayout::SplitV)
        );
        assert_eq!(
            workspace.floating().root_layout_for_window(&2),
            Some(ContainerLayout::SplitV)
        );
    }

    check_ops_on_layout(
        &mut layout,
        [Op::FocusParent, Op::ToggleWindowFloating { id: None }],
    );

    let workspace = layout.active_workspace().expect("active workspace");
    assert!(!workspace.floating_is_active());
    assert_eq!(workspace.floating().tiles().count(), 0);
    assert_eq!(workspace.tiling().tiles().count(), 2);
}
#[test]
fn floating_split_after_refocus_targets_refocused_window() {
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
        Op::AddWindow {
            params: TestWindowParams::new(3),
        },
        Op::Communicate(1),
        Op::Communicate(2),
        Op::Communicate(3),
        Op::CompleteAnimations,
    ]);

    check_ops_on_layout(
        &mut layout,
        [
            Op::FocusWindow(1),
            Op::SplitHorizontal,
            Op::AddWindow {
                params: TestWindowParams::new(4),
            },
            Op::Communicate(4),
            Op::CompleteAnimations,
        ],
    );

    let workspace = layout.active_workspace().expect("active workspace");
    assert!(workspace.is_floating(&4));

    let r1 = tile_rect(&layout, 1);
    let r2 = tile_rect(&layout, 2);
    let r3 = tile_rect(&layout, 3);
    let r4 = tile_rect(&layout, 4);

    // After refocusing window 1 and splitting horizontally, window 4 should
    // be inserted alongside window 1 (top split), not near the previously
    // focused last window.
    assert!((r4.loc.y - r1.loc.y).abs() <= 1.0);
    assert!(r4.loc.y + 1.0 < r2.loc.y);
    assert!(r4.loc.y + 1.0 < r3.loc.y);
}
#[test]
fn floating_initial_size_is_stable_across_focus_changes_and_width_resize() {
    let mut layout = check_ops([
        Op::AddOutput(1),
        Op::AddOutput(2),
        Op::AddWindow {
            params: TestWindowParams::new(1),
        },
        Op::ToggleWindowFloating { id: None },
    ]);

    let initial_size = requested_size(&layout, 1);
    assert_eq!(
        initial_size,
        Size::from((640, 540)),
        "first floating request should use the deterministic 50% x 75% preset"
    );

    check_ops_on_layout(&mut layout, [Op::FocusOutput(2), Op::FocusOutput(1)]);

    assert_eq!(
        requested_size(&layout, 1),
        initial_size,
        "output focus changes should not mutate stored initial floating size"
    );

    check_ops_on_layout(
        &mut layout,
        [Op::SetWindowWidth {
            id: Some(1),
            change: SizeChange::SetFixed(500),
        }],
    );

    let resized = requested_size(&layout, 1);
    assert_eq!(resized.w, 500);
    assert_eq!(
        resized.h, initial_size.h,
        "explicit width resize should keep current floating height"
    );
}
#[test]
fn floating_toggle_single_selected_container_moves_to_tiling() {
    let mut layout = check_ops([
        Op::AddOutput(1),
        Op::AddWindow {
            params: TestWindowParams::new(1),
        },
        Op::ToggleWindowFloating { id: None },
        Op::SplitVertical,
        Op::FocusParent,
    ]);

    {
        let workspace = layout.active_workspace().expect("active workspace");
        assert!(workspace.floating_is_active());
        let focus_id = layout
            .focus()
            .map(|window| *window.id())
            .expect("focused window");
        assert!(
            workspace.floating().selected_is_container(Some(&focus_id)),
            "test precondition: expected floating container selection before toggle"
        );
    }

    layout.toggle_window_floating(None);

    let workspace = layout.active_workspace().expect("active workspace");
    assert!(
        !workspace.floating_is_active(),
        "toggle_floating on a single-window floating container selection should switch to tiling"
    );
    assert_eq!(workspace.floating().tiles().count(), 0);
    assert_eq!(workspace.tiling().tiles().count(), 1);
}
#[test]
fn floating_toggle_multi_window_selected_container_moves_to_tiling() {
    let mut layout = check_ops([
        Op::AddOutput(1),
        Op::AddWindow {
            params: TestWindowParams::new(1),
        },
        Op::AddWindow {
            params: TestWindowParams::new(2),
        },
        Op::FocusWindow(1),
        Op::ToggleWindowFloating { id: None },
        Op::SplitVertical,
        Op::AddWindow {
            params: TestWindowParams::new(3),
        },
        Op::AddWindow {
            params: TestWindowParams::new(4),
        },
        Op::FocusParent,
    ]);

    let selected_ids = layout.close_window_ids_for_active_selection();
    assert!(
        selected_ids.len() >= 3,
        "test precondition: expected multi-window floating container selection before toggle"
    );

    {
        let workspace = layout.active_workspace().expect("active workspace");
        assert!(workspace.floating_is_active());
        let focus_id = layout
            .focus()
            .map(|window| *window.id())
            .expect("focused window");
        assert!(
            workspace.floating().selected_is_container(Some(&focus_id)),
            "test precondition: expected floating container selection before toggle"
        );
    }

    layout.toggle_window_floating(None);

    let workspace = layout.active_workspace().expect("active workspace");
    assert!(
        !workspace.floating_is_active(),
        "toggle_floating on a multi-window floating container selection should switch to tiling",
    );
    for id in selected_ids {
        assert!(
            !workspace.is_floating(&id),
            "window {id} should be restored to tiling when toggling selected floating container",
        );
    }
}
#[test]
fn floating_toggle_selected_tiling_container_roundtrips_through_workspace_context() {
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
        Op::FocusParent,
    ]);

    let tree_before = layout
        .active_workspace()
        .expect("active workspace")
        .tiling()
        .debug_tree()
        .replace(" *", "");
    let selected_ids = layout.close_window_ids_for_active_selection();
    assert_eq!(
        selected_ids,
        vec![3, 4],
        "precondition: focus-parent should select the nested tiling container",
    );

    layout.toggle_window_floating(None);

    {
        let workspace = layout.active_workspace().expect("active workspace");
        assert!(workspace.floating_is_active());
        assert_eq!(workspace.tiling().tiles().count(), 2);
        assert_eq!(workspace.floating().tiles().count(), 2);
        for id in &selected_ids {
            assert!(
                workspace.is_floating(id),
                "window {id} should move into the floating container during the first toggle",
            );
        }
    }

    check_ops_on_layout(
        &mut layout,
        [
            Op::FocusParent,
            Op::FocusParent,
            Op::ToggleWindowFloating { id: None },
        ],
    );

    let workspace = layout.active_workspace().expect("active workspace");
    let tree_after = workspace.tiling().debug_tree().replace(" *", "");
    assert!(
        !workspace.floating_is_active(),
        "toggle_floating from floating workspace-context should restore the subtree to tiling",
    );
    assert_eq!(workspace.floating().tiles().count(), 0);
    assert_eq!(workspace.tiling().tiles().count(), 4);
    assert_eq!(
        tree_after, tree_before,
        "the restored tiling tree should match the original subtree layout after the full roundtrip",
    );
    for id in selected_ids {
        assert!(
            !workspace.is_floating(&id),
            "window {id} should return to tiling after the second toggle",
        );
    }
}
#[test]
fn floating_toggle_workspace_subtree_roundtrips_all_windows_back_to_tiling() {
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
        Op::FocusParent,
        Op::FocusParent,
    ]);

    let tree_before = layout
        .active_workspace()
        .expect("active workspace")
        .tiling()
        .debug_tree()
        .replace(" *", "");
    let selected_ids = layout.close_window_ids_for_active_selection();
    assert_eq!(
        selected_ids,
        vec![1, 2, 3],
        "precondition: focus-parent twice should target the whole tiling workspace subtree",
    );

    layout.toggle_window_floating(None);

    {
        let workspace = layout.active_workspace().expect("active workspace");
        assert!(workspace.floating_is_active());
        assert_eq!(workspace.tiling().tiles().count(), 0);
        assert_eq!(workspace.floating().tiles().count(), 3);
        for id in &selected_ids {
            assert!(
                workspace.is_floating(id),
                "window {id} should move into the floating workspace subtree during the first toggle",
            );
        }
    }

    check_ops_on_layout(
        &mut layout,
        [Op::FocusParent, Op::ToggleWindowFloating { id: None }],
    );

    let workspace = layout.active_workspace().expect("active workspace");
    let tree_after = workspace.tiling().debug_tree().replace(" *", "");
    assert!(
        !workspace.floating_is_active(),
        "unfloating an all-windows workspace subtree should return focus mode to tiling",
    );
    assert_eq!(workspace.floating().tiles().count(), 0);
    assert_eq!(workspace.tiling().tiles().count(), 3);
    assert_eq!(
        tree_after, tree_before,
        "restoring the whole workspace subtree should recover the original tiling tree",
    );
    for id in selected_ids {
        assert!(
            !workspace.is_floating(&id),
            "window {id} should return to tiling after restoring the whole workspace subtree",
        );
    }
}
#[test]
fn floating_single_window_roundtrip_does_not_reintroduce_implicit_split_wrapper() {
    let mut layout = check_ops([
        Op::AddOutput(1),
        Op::AddWindow {
            params: TestWindowParams::new(1),
        },
    ]);

    // Defer a split hint on single tiling leaf, then roundtrip through floating.
    layout.split_horizontal();
    layout.toggle_window_floating(None);
    layout.focus_up();
    layout.set_layout_mode(ContainerLayout::Stacked);
    layout.set_layout_mode(ContainerLayout::SplitV);
    layout.toggle_window_floating(None);

    {
        let workspace = layout.active_workspace().expect("active workspace");
        assert!(!workspace.floating_is_active());
        assert_eq!(workspace.floating().tiles().count(), 0);
        assert_eq!(workspace.tiling().tiles().count(), 1);
        let tree = workspace.tiling().debug_tree();
        assert!(
            !tree.contains("SplitH")
                && !tree.contains("SplitV")
                && !tree.contains("Tabbed")
                && !tree.contains("Stacked"),
            "floating->tiling roundtrip for a single implicit container should restore a leaf root:\n{tree}",
        );
    }

    // Toggling back to floating should now match sway semantics (no hidden split wrapper in tiling).
    layout.toggle_window_floating(None);
    let workspace = layout.active_workspace().expect("active workspace");
    assert!(workspace.floating_is_active());
    assert_eq!(workspace.floating().tiles().count(), 1);
    assert_eq!(workspace.tiling().tiles().count(), 0);
}
#[test]
fn workspace_split_from_workspace_context_keeps_floating_mode_like_sway() {
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
        Op::FocusParent,
    ]);

    {
        let workspace = layout.active_workspace().expect("active workspace");
        assert!(
            workspace.floating_is_active(),
            "precondition: floating mode must be active"
        );
        assert!(
            workspace.debug_floating_workspace_context(),
            "precondition: focus_parent on floating leaf should put us in workspace context",
        );
        assert_eq!(workspace.tiling().tiles().count(), 1);
    }

    layout.split_horizontal();

    let workspace = layout.active_workspace().expect("active workspace");
    assert!(
        workspace.floating_is_active(),
        "workspace split in floating workspace-context should keep floating mode (sway parity)",
    );
    assert!(
        workspace.debug_floating_workspace_context(),
        "workspace split in this path should keep workspace command context",
    );
}
#[test]
fn floating_focus_parent_ignores_redundant_single_child_wrapper() {
    let layout = check_ops([
        Op::AddOutput(1),
        Op::AddWindow {
            params: TestWindowParams::new(1),
        },
        Op::ToggleWindowFloating { id: None },
        Op::FocusParent,
    ]);

    let workspace = layout.active_workspace().expect("active workspace");
    assert!(workspace.is_floating(&1));
    assert!(!workspace.floating().wrapper_selected_for_window(&1));
    assert!(!workspace.floating().selected_is_container(Some(&1)));
    assert!(workspace.floating_is_active());
}
#[test]
fn floating_focus_parent_at_wrapper_keeps_floating_mode() {
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
        Op::FocusParent,
        Op::ToggleWindowFloating { id: None },
    ]);

    {
        let workspace = layout.active_workspace().expect("active workspace");
        assert!(workspace.floating_is_active());
        assert!(workspace.is_floating(&2));
        assert!(!workspace.is_floating(&1));
    }

    check_ops_on_layout(&mut layout, [Op::FocusParent]);

    let workspace = layout.active_workspace().expect("active workspace");
    assert!(
        workspace.floating_is_active(),
        "focus_parent at floating wrapper should keep floating mode (sway parity)",
    );
}
#[test]
fn floating_explicit_split_returns_to_tiling_as_container() {
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
            Op::ToggleWindowFloating { id: None },
            Op::SplitHorizontal,
            Op::ToggleWindowFloating { id: None },
        ],
    );

    let workspace = layout.active_workspace().expect("active workspace");
    assert!(!workspace.is_floating(&2));
    let tree_after_return = workspace.tiling().debug_tree().replace(" *", "");
    assert!(
        tree_after_return.contains("SplitH\n  Window 1\n  SplitH\n    Window 2"),
        "floating split should return as nested tiling container:\n{tree_after_return}"
    );

    check_ops_on_layout(
        &mut layout,
        [Op::AddWindow {
            params: TestWindowParams::new(3),
        }],
    );

    let workspace = layout.active_workspace().expect("active workspace");
    let tree_after_insert = workspace.tiling().debug_tree().replace(" *", "");
    assert!(
        tree_after_insert.contains("SplitH\n  Window 1\n  SplitH\n    Window 2\n    Window 3")
            || tree_after_insert
                .contains("SplitH\n  Window 1\n  SplitH\n    Window 3\n    Window 2"),
        "new tiling window should insert inside preserved split container:\n{tree_after_insert}"
    );
}
#[test]
fn floating_to_tiling_restore_uses_leaf_reference_as_sibling() {
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
        Op::FocusWindow(1),
        Op::ToggleWindowFloating { id: Some(3) },
        Op::ToggleWindowFloating { id: Some(3) },
    ]);

    let workspace = layout.active_workspace().expect("active workspace");
    assert!(!workspace.is_floating(&3));

    layout.activate_window(&1);
    let idx1 = layout
        .active_workspace()
        .expect("active workspace")
        .tiling()
        .focus_path();
    layout.activate_window(&3);
    let idx3 = layout
        .active_workspace()
        .expect("active workspace")
        .tiling()
        .focus_path();
    layout.activate_window(&2);
    let idx2 = layout
        .active_workspace()
        .expect("active workspace")
        .tiling()
        .focus_path();

    assert_eq!(
        idx1.len(),
        1,
        "window 1 should remain a root child: {idx1:?}"
    );
    assert_eq!(
        idx3.len(),
        1,
        "window 3 should be inserted as a root sibling: {idx3:?}"
    );
    assert_eq!(
        idx2.len(),
        1,
        "window 2 should remain a root child: {idx2:?}"
    );
    assert!(
        idx1[0] < idx3[0] && idx3[0] < idx2[0],
        "leaf reference restore should insert after window 1 and before window 2: {idx1:?} {idx3:?} {idx2:?}"
    );
}
#[test]
fn floating_to_tiling_restore_uses_container_reference_as_child() {
    let mut layout = check_ops([
        Op::AddOutput(1),
        Op::AddWindow {
            params: TestWindowParams::new(1),
        },
        Op::AddWindow {
            params: TestWindowParams::new(3),
        },
        Op::FocusWindow(1),
        Op::SplitVertical,
        Op::AddWindow {
            params: TestWindowParams::new(4),
        },
        Op::FocusWindow(3),
        Op::ToggleWindowFloating { id: Some(3) },
        Op::FocusTiling,
        Op::FocusWindow(1),
        Op::FocusParent,
        Op::ToggleWindowFloating { id: Some(3) },
    ]);

    let workspace = layout.active_workspace().expect("active workspace");
    assert!(!workspace.is_floating(&3));

    layout.activate_window(&1);
    let path1 = layout
        .active_workspace()
        .expect("active workspace")
        .tiling()
        .focus_path();
    layout.activate_window(&4);
    let path4 = layout
        .active_workspace()
        .expect("active workspace")
        .tiling()
        .focus_path();
    layout.activate_window(&3);
    let path3 = layout
        .active_workspace()
        .expect("active workspace")
        .tiling()
        .focus_path();

    assert!(
        path1.len() == path4.len() && path4.len() == path3.len(),
        "all windows should stay in the same restored container depth: {path1:?} {path4:?} {path3:?}"
    );
    assert_eq!(
        &path1[..path1.len() - 1],
        &path4[..path4.len() - 1],
        "window 4 should remain under the same container as window 1: {path1:?} {path4:?}"
    );
    assert_eq!(
        &path1[..path1.len() - 1],
        &path3[..path3.len() - 1],
        "restored window should be inserted as a child of the selected container: {path1:?} {path3:?}"
    );
    assert!(
        path1[path1.len() - 1] < path4[path4.len() - 1]
            && path4[path4.len() - 1] < path3[path3.len() - 1],
        "container-reference restore should append after existing children (1,4,3): {path1:?} {path4:?} {path3:?}"
    );
}
#[test]
fn floating_stacked_then_split_roundtrip_preserves_container() {
    let layout = check_ops([
        Op::AddOutput(1),
        Op::AddWindow {
            params: TestWindowParams::new(1),
        },
        Op::AddWindow {
            params: TestWindowParams::new(2),
        },
        Op::ToggleWindowFloating { id: None },
        Op::FocusChild,
        Op::FocusChild,
        Op::SetLayoutStacked,
        Op::SplitHorizontal,
        Op::FocusWindowUp,
        Op::ToggleWindowFloating { id: None },
        Op::AddWindow {
            params: TestWindowParams::new(3),
        },
    ]);

    let workspace = layout.active_workspace().expect("active workspace");
    assert!(!workspace.is_floating(&2));
    assert!(!workspace.is_floating(&3));
    let tree = workspace.tiling().debug_tree().replace(" *", "");
    assert!(
        tree.contains("SplitH\n  Window 1\n  SplitH\n    Window 2\n    Window 3")
            || tree.contains("SplitH\n  Window 1\n  SplitH\n    Window 3\n    Window 2"),
        "expected nested split container after floating roundtrip:\n{tree}"
    );
}
#[test]
fn floating_toggle_after_split_marks_container_as_grouped() {
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
            Op::FocusWindowUp,
            Op::CloseWindow(4),
            Op::FocusColumnRight,
            Op::SplitVertical,
            Op::FocusWindowUp,
            Op::ToggleWindowFloating { id: None },
            Op::FocusChild,
            Op::FocusChild,
            Op::SetLayoutStacked,
            Op::SplitHorizontal,
            Op::FocusWindowUp,
        ],
    );

    let workspace = layout.active_workspace().expect("active workspace");
    let floating_id = *workspace
        .floating()
        .active_window()
        .expect("floating window should stay active")
        .id();
    assert!(workspace.is_floating(&floating_id));
    assert!(
        workspace.floating_container_allows_splits(&floating_id),
        "floating explicit split should be considered grouped for toggle back"
    );

    check_ops_on_layout(&mut layout, [Op::ToggleWindowFloating { id: None }]);
    let workspace = layout.active_workspace().expect("active workspace");
    let tree = workspace.tiling().debug_tree().replace(" *", "");
    let has_single_leaf_split = tree.contains("\n  SplitH\n    Window ");
    assert!(
        has_single_leaf_split,
        "expected explicit floating split to return as single-leaf split container:\n{tree}"
    );
}
#[test]
fn floating_focus_parent_reaches_wrapper_after_root_in_nested_tree() {
    let mut params2 = TestWindowParams::new(2);
    params2.is_floating = true;
    let mut params3 = TestWindowParams::new(3);
    params3.is_floating = true;

    let layout = check_ops([
        Op::AddOutput(1),
        Op::AddWindow {
            params: TestWindowParams::new(1),
        },
        Op::ToggleWindowFloating { id: None },
        Op::SplitVertical,
        Op::AddWindow { params: params2 },
        Op::FocusWindow(1),
        Op::SplitHorizontal,
        Op::AddWindow { params: params3 },
        Op::FocusWindow(1),
        Op::FocusParent,
    ]);

    let workspace = layout.active_workspace().expect("active workspace");
    assert!(workspace.is_floating(&1));
    assert!(!workspace.floating().wrapper_selected_for_window(&1));

    let mut layout = layout;
    check_ops_on_layout(&mut layout, [Op::FocusParent]);
    let workspace = layout.active_workspace().expect("active workspace");
    assert!(workspace.floating().wrapper_selected_for_window(&1));
}
#[test]
fn floating_focus_child_exits_wrapper_selection() {
    let layout = check_ops([
        Op::AddOutput(1),
        Op::AddWindow {
            params: TestWindowParams::new(1),
        },
        Op::ToggleWindowFloating { id: None },
        Op::FocusParent,
        Op::FocusParent,
        Op::FocusChild,
    ]);

    let workspace = layout.active_workspace().expect("active workspace");
    assert!(workspace.is_floating(&1));
    assert!(!workspace.floating().wrapper_selected_for_window(&1));
}
#[test]
fn floating_split_with_wrapper_selected_changes_root_layout() {
    let mut params2 = TestWindowParams::new(2);
    params2.is_floating = true;

    let layout = check_ops([
        Op::AddOutput(1),
        Op::AddWindow {
            params: TestWindowParams::new(1),
        },
        Op::ToggleWindowFloating { id: None },
        Op::SplitVertical,
        Op::AddWindow { params: params2 },
        Op::FocusWindow(1),
        Op::FocusParent,
        Op::FocusParent,
        Op::SplitHorizontal,
    ]);

    let workspace = layout.active_workspace().expect("active workspace");
    assert!(workspace.is_floating(&1));
    assert!(workspace.is_floating(&2));
    assert_eq!(
        workspace.floating().root_layout_for_window(&1),
        Some(ContainerLayout::SplitH)
    );
}
#[test]
fn floating_set_layout_mode_on_wrapper_is_noop_like_sway() {
    let mut params2 = TestWindowParams::new(2);
    params2.is_floating = true;

    let layout = check_ops([
        Op::AddOutput(1),
        Op::AddWindow {
            params: TestWindowParams::new(1),
        },
        Op::ToggleWindowFloating { id: None },
        Op::SplitVertical,
        Op::AddWindow { params: params2 },
        Op::FocusParent,
        Op::FocusParent,
        Op::SetLayoutTabbed,
    ]);

    let workspace = layout.active_workspace().expect("active workspace");
    assert!(workspace.is_floating(&1));
    assert!(workspace.is_floating(&2));
    assert_eq!(
        workspace.floating().root_layout_for_window(&1),
        Some(ContainerLayout::SplitV)
    );
}
#[test]
fn floating_consume_into_column_uses_floating_tree() {
    let layout = check_ops([
        Op::AddOutput(1),
        Op::AddWindow {
            params: TestWindowParams::new(1),
        },
        Op::ToggleWindowFloating { id: None },
        Op::ConsumeWindowIntoColumn,
    ]);

    let workspace = layout.active_workspace().expect("active workspace");
    assert!(workspace.is_floating(&1));
    assert_eq!(
        workspace.floating().root_layout_for_window(&1),
        Some(ContainerLayout::SplitV)
    );
}
#[test]
fn floating_expel_from_column_uses_floating_tree() {
    let layout = check_ops([
        Op::AddOutput(1),
        Op::AddWindow {
            params: TestWindowParams::new(1),
        },
        Op::ToggleWindowFloating { id: None },
        Op::ExpelWindowFromColumn,
    ]);

    let workspace = layout.active_workspace().expect("active workspace");
    assert!(workspace.is_floating(&1));
    assert_eq!(
        workspace.floating().root_layout_for_window(&1),
        Some(ContainerLayout::SplitH)
    );
}
#[test]
fn consume_or_expel_targeting_floating_window_does_not_use_tiling_tree() {
    let layout = check_ops([
        Op::AddOutput(1),
        Op::AddWindow {
            params: TestWindowParams::new(1),
        },
        Op::ToggleWindowFloating { id: None },
        Op::AddWindow {
            params: TestWindowParams::new(2),
        },
        Op::ConsumeOrExpelWindowLeft { id: Some(1) },
    ]);

    let workspace = layout.active_workspace().expect("active workspace");
    assert!(workspace.is_floating(&1));
    assert!(!workspace.is_floating(&2));
    assert_eq!(
        workspace.floating().root_layout_for_window(&1),
        Some(ContainerLayout::SplitV)
    );
    assert!(window_layout(&layout, 2).pos_in_tiling_layout.is_some());
}
#[test]
fn floating_toggle_column_tabbed_display_changes_floating_layout() {
    let layout = check_ops([
        Op::AddOutput(1),
        Op::AddWindow {
            params: TestWindowParams::new(1),
        },
        Op::ToggleWindowFloating { id: None },
        Op::SplitVertical,
        Op::ToggleColumnTabbedDisplay,
    ]);

    let workspace = layout.active_workspace().expect("active workspace");
    assert!(workspace.is_floating(&1));
    assert_eq!(
        workspace.floating().root_layout_for_window(&1),
        Some(ContainerLayout::Tabbed)
    );
}
#[test]
fn floating_tab_bar_hit_does_not_report_resize_edges() {
    let mut layout = Layout::default();
    let output = make_test_output("output-test");
    layout.add_output(output.clone(), None);

    layout.add_window(
        TestWindow::new(TestWindowParams::new(1)),
        AddWindowTarget::Auto,
        None,
        None,
        false,
        false,
        ActivateWindow::Yes,
    );
    layout.toggle_window_floating(None);
    layout.split_vertical();
    layout.add_window(
        TestWindow::new(TestWindowParams::new(2)),
        AddWindowTarget::NextTo(&1),
        None,
        None,
        false,
        false,
        ActivateWindow::Yes,
    );
    layout.toggle_column_tabbed_display();

    {
        let workspace = layout.active_workspace().expect("active workspace");
        assert!(workspace.is_floating(&1));
        assert!(workspace.is_floating(&2));
        assert_eq!(
            workspace.floating().root_layout_for_window(&1),
            Some(ContainerLayout::Tabbed)
        );
    }

    let rect = tile_rect(&layout, 2);
    let mut tab_pos = None;
    for dy in 1..96 {
        for frac in [0.2, 0.5, 0.8] {
            let candidate = rect.loc + Point::from((rect.size.w * frac, -(dy as f64)));
            if matches!(
                layout.window_under(&output, candidate),
                Some((
                    _,
                    HitType::Activate {
                        is_tab_indicator: true
                    }
                ))
            ) {
                tab_pos = Some(candidate);
                break;
            }
        }
        if tab_pos.is_some() {
            break;
        }
    }

    let tab_pos = tab_pos.expect("expected a tab-bar hit position above floating tile");
    assert_eq!(layout.resize_edges_under(&output, tab_pos), None);

    let mut tab_pos_top = None;
    for dy in (1..96).rev() {
        for frac in [0.2, 0.5, 0.8] {
            let candidate = rect.loc + Point::from((rect.size.w * frac, -(dy as f64)));
            if matches!(
                layout.window_under(&output, candidate),
                Some((
                    _,
                    HitType::Activate {
                        is_tab_indicator: true
                    }
                ))
            ) {
                tab_pos_top = Some(candidate);
                break;
            }
        }
        if tab_pos_top.is_some() {
            break;
        }
    }

    let tab_pos_top = tab_pos_top.expect("expected a top tab-bar hit position above floating tile");
    assert_eq!(layout.resize_edges_under(&output, tab_pos_top), None);
}
#[test]
fn floating_tab_bar_hit_does_not_fall_through_to_tiling_window() {
    let mut layout = Layout::default();
    let output = make_test_output("output-test");
    layout.add_output(output.clone(), None);

    layout.add_window(
        TestWindow::new(TestWindowParams::new(1)),
        AddWindowTarget::Auto,
        None,
        None,
        false,
        false,
        ActivateWindow::Yes,
    );
    layout.add_window(
        TestWindow::new(TestWindowParams::new(2)),
        AddWindowTarget::Auto,
        None,
        None,
        false,
        false,
        ActivateWindow::Yes,
    );
    layout.toggle_window_floating(None);
    layout.split_vertical();
    layout.add_window(
        TestWindow::new(TestWindowParams::new(3)),
        AddWindowTarget::NextTo(&2),
        None,
        None,
        false,
        false,
        ActivateWindow::Yes,
    );
    layout.toggle_column_tabbed_display();

    let rect = tile_rect(&layout, 3);
    let mut hit = None;
    for dy in 1..96 {
        for frac in [0.2, 0.5, 0.8] {
            let candidate = rect.loc + Point::from((rect.size.w * frac, -(dy as f64)));
            if let Some((
                win,
                HitType::Activate {
                    is_tab_indicator: true,
                },
            )) = layout.window_under(&output, candidate)
            {
                if *win.id() != 1 {
                    hit = Some((candidate, *win.id()));
                    break;
                }
            }
        }
        if hit.is_some() {
            break;
        }
    }

    let (candidate, id) = hit.expect("expected floating tab bar hit to capture pointer");
    assert_ne!(
        id, 1,
        "tab bar hit must not fall through to tiling window below"
    );
    assert_eq!(layout.resize_edges_under(&output, candidate), None);
}
#[test]
fn toggle_window_floating_after_output_attach_keeps_options_synced() {
    check_ops([
        Op::AddWindow {
            params: TestWindowParams::new(1),
        },
        Op::AddOutput(1),
        Op::FocusParent,
        Op::ToggleWindowFloating { id: None },
    ]);
}
#[test]
fn move_window_to_workspace_up_after_maximize_keeps_floating_normal() {
    let ops = [
        Op::AddWindow {
            params: TestWindowParams {
                id: 3,
                is_floating: true,
                ..TestWindowParams::new(3)
            },
        },
        Op::AddWindow {
            params: TestWindowParams::new(1),
        },
        Op::AddOutput(1),
        Op::MoveWindowToWorkspace {
            window_id: None,
            workspace_idx: 1,
        },
        Op::MaximizeWindowToEdges { id: None },
        Op::MoveWindowToWorkspaceUp(false),
    ];

    let layout = check_ops(ops);

    let monitor = match layout.monitor_set {
        MonitorSet::Normal { monitors, .. } => monitors.into_iter().next().unwrap(),
        MonitorSet::NoOutputs { .. } => unreachable!(),
    };

    // Window 1 was maximized before the move and should stay in tiling (not floating).
    let ws0 = &monitor.workspaces[0];
    assert!(ws0.tiling().tiles().any(|tile| tile.window().id() == &1));
    assert!(!ws0.floating().tiles().any(|tile| tile.window().id() == &1));
}
#[test]
fn interactive_move_toggle_floating_ends_dnd_gesture() {
    let ops = [
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
        Op::Refresh { is_active: false },
        Op::ToggleWindowFloating { id: None },
        Op::InteractiveMoveEnd { window: 2 },
    ];

    check_ops(ops);
}
#[test]
fn interactive_move_floating_window_stays_out_of_active_grouped_floating_container() {
    let mut layout = check_ops([
        Op::AddOutput(1),
        Op::AddWindow {
            params: TestWindowParams::new(1),
        },
        Op::AddWindow {
            params: TestWindowParams::new(2),
        },
        Op::FocusWindow(1),
        Op::ToggleWindowFloating { id: None },
        Op::SplitVertical,
        Op::AddWindow {
            params: TestWindowParams::new(3),
        },
        Op::AddWindow {
            params: TestWindowParams::new(4),
        },
        Op::FocusWindow(2),
        Op::ToggleWindowFloating { id: None },
    ]);

    {
        let workspace = layout.active_workspace().expect("active workspace");
        let window2_tree = workspace
            .floating()
            .debug_tree_for_window(&2)
            .expect("window 2 floating tree");
        assert_eq!(workspace.tiling().tiles().count(), 0);
        assert_eq!(workspace.floating().tiles().count(), 4);
        assert_eq!(
            workspace.floating().root_layout_for_window(&1),
            Some(ContainerLayout::SplitV)
        );
        assert_eq!(
            window2_tree.matches("Window ").count(),
            1,
            "precondition: window 2 should start in its own floating container:\n{window2_tree}",
        );
    }

    check_ops_on_layout(
        &mut layout,
        [
            Op::InteractiveMoveBegin {
                window: 2,
                output_idx: 1,
                px: 0.,
                py: 0.,
            },
            Op::InteractiveMoveUpdate {
                window: 2,
                dx: 1.,
                dy: 0.,
                output_idx: 1,
                px: 1.,
                py: 0.,
            },
            Op::InteractiveMoveEnd { window: 2 },
        ],
    );

    let workspace = layout.active_workspace().expect("active workspace");
    let window2_tree = workspace
        .floating()
        .debug_tree_for_window(&2)
        .expect("window 2 floating tree");
    assert_eq!(workspace.tiling().tiles().count(), 0);
    assert_eq!(workspace.floating().tiles().count(), 4);
    assert_eq!(
        workspace.floating().root_layout_for_window(&1),
        Some(ContainerLayout::SplitV)
    );
    assert_eq!(
        window2_tree.matches("Window ").count(),
        1,
        "interactive move should keep window 2 in its own floating container:\n{window2_tree}",
    );
}
#[test]
fn interactive_move_floating_window_stays_out_of_toggled_floating_subtree() {
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
        Op::FocusParent,
        Op::ToggleWindowFloating { id: None },
        Op::FocusWindow(1),
        Op::ToggleWindowFloating { id: None },
    ]);

    {
        let workspace = layout.active_workspace().expect("active workspace");
        let window1_tree = workspace
            .floating()
            .debug_tree_for_window(&1)
            .expect("window 1 floating tree");
        assert_eq!(workspace.tiling().tiles().count(), 1);
        assert_eq!(workspace.floating().tiles().count(), 3);
        assert_eq!(
            window1_tree.matches("Window ").count(),
            1,
            "precondition: window 1 should start in its own floating container:\n{window1_tree}",
        );
        let window4_tree = workspace
            .floating()
            .debug_tree_for_window(&4)
            .expect("window 4 floating tree");
        assert!(
            window4_tree.matches("Window ").count() >= 2,
            "precondition: window 4 should belong to a grouped floating subtree:\n{window4_tree}",
        );
    }

    check_ops_on_layout(
        &mut layout,
        [
            Op::InteractiveMoveBegin {
                window: 1,
                output_idx: 1,
                px: 0.,
                py: 0.,
            },
            Op::InteractiveMoveUpdate {
                window: 1,
                dx: 1.,
                dy: 0.,
                output_idx: 1,
                px: 1.,
                py: 0.,
            },
            Op::InteractiveMoveEnd { window: 1 },
        ],
    );

    let workspace = layout.active_workspace().expect("active workspace");
    let window1_tree = workspace
        .floating()
        .debug_tree_for_window(&1)
        .expect("window 1 floating tree");
    assert_eq!(workspace.tiling().tiles().count(), 1);
    assert_eq!(workspace.floating().tiles().count(), 3);
    assert_eq!(
        window1_tree.matches("Window ").count(),
        1,
        "interactive move should keep window 1 in its own floating container:\n{window1_tree}",
    );
}
#[test]
fn move_column_to_workspace_down_focus_false_on_floating_window() {
    let ops = [
        Op::AddOutput(1),
        Op::AddWindow {
            params: TestWindowParams::new(1),
        },
        Op::AddWindow {
            params: TestWindowParams::new(2),
        },
        Op::ToggleWindowFloating { id: None },
        Op::MoveColumnToWorkspaceDown(false),
    ];

    let layout = check_ops(ops);

    let MonitorSet::Normal { monitors, .. } = layout.monitor_set else {
        unreachable!()
    };

    assert_eq!(monitors[0].active_workspace_idx, 0);
}
#[test]
fn move_column_to_workspace_focus_false_on_floating_window() {
    let ops = [
        Op::AddOutput(1),
        Op::AddWindow {
            params: TestWindowParams::new(1),
        },
        Op::AddWindow {
            params: TestWindowParams::new(2),
        },
        Op::ToggleWindowFloating { id: None },
        Op::MoveColumnToWorkspace(1, false),
    ];

    let layout = check_ops(ops);

    let MonitorSet::Normal { monitors, .. } = layout.monitor_set else {
        unreachable!()
    };

    assert_eq!(monitors[0].active_workspace_idx, 0);
}
#[test]
fn tiling_maximized_window_floated_clears_maximized_state() {
    let ops = [
        Op::AddWindow {
            params: TestWindowParams::new(3),
        },
        Op::AddWindow {
            params: TestWindowParams::new(4),
        },
        Op::MaximizeWindowToEdges { id: Some(3) },
        Op::AddOutput(1),
        Op::FocusParent,
        Op::ToggleWindowFloating { id: None },
    ];

    let layout = check_ops(ops);

    let workspace = layout.active_workspace().unwrap();
    assert!(workspace.is_floating(&3));

    let (_mon, win3) = layout
        .windows()
        .find(|(_, win)| *win.id() == 3)
        .expect("window 3 should exist");
    assert!(win3.pending_sizing_mode().is_normal());
}
#[test]
fn floating_interactive_resize_then_unfloat_clears_resize_state() {
    let ops = [
        Op::AddWindow {
            params: TestWindowParams {
                id: 5,
                is_floating: true,
                ..TestWindowParams::new(5)
            },
        },
        Op::AddOutput(1),
        Op::InteractiveResizeBegin {
            window: 5,
            edges: ResizeEdge::RIGHT,
        },
        Op::ToggleWindowFloating { id: None },
    ];

    let layout = check_ops(ops);
    let workspace = layout.active_workspace().unwrap();

    assert!(!workspace.is_floating(&5));
}
#[test]
fn kill_selected_floating_container_does_not_close_other_windows() {
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
        Op::FocusWindow(2),
        Op::ToggleWindowFloating { id: None },
        Op::SplitVertical,
        Op::AddWindow {
            params: TestWindowParams::new(4),
        },
        Op::FocusWindow(3),
        Op::ToggleWindowFloating { id: None },
        Op::FocusWindow(2),
        Op::FocusParent,
    ]);

    let workspace = layout.active_workspace().expect("active workspace");
    assert!(workspace.floating_is_active());
    assert_eq!(workspace.tiling().tiles().count(), 1);
    assert_eq!(workspace.floating().tiles().count(), 3);
    assert!(
        workspace.debug_command_target() == "floating_container",
        "precondition: expected floating container selection",
    );

    let mut selected_ids = layout.close_window_ids_for_active_selection();
    selected_ids.sort_unstable();
    assert_eq!(
        selected_ids,
        vec![2, 4],
        "killing a selected floating container should not close other floating or tiling windows",
    );
}
#[test]
fn focusing_floating_leaf_clears_container_selection_and_restores_leaf_navigation() {
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
        Op::ToggleWindowFloating { id: None },
        Op::FocusWindow(2),
    ]);

    {
        let workspace = layout.active_workspace().expect("active workspace");
        assert!(workspace.floating_is_active());
        assert_eq!(workspace.debug_command_target(), "floating_window");
        assert!(!workspace.debug_floating_workspace_context());
        assert!(!workspace.debug_active_floating_wrapper_selected());
        assert!(
            !workspace.floating().selected_is_container(Some(&2)),
            "explicitly focusing a floating leaf should clear floating container selection",
        );
    }

    layout.focus_right();
    assert_eq!(
        layout.focus().map(|win| *win.id()),
        Some(3),
        "after focusing a floating leaf, directional focus should move between sibling floating windows again",
    );

    layout.focus_parent();
    {
        let workspace = layout.active_workspace().expect("active workspace");
        assert_eq!(workspace.debug_command_target(), "floating_container");
    }

    layout.toggle_window_floating(None);
    let workspace = layout.active_workspace().expect("active workspace");
    assert!(!workspace.floating_is_active());
    assert_eq!(workspace.floating().tiles().count(), 0);
    assert_eq!(workspace.tiling().tiles().count(), 3);
}
#[test]
fn floating_workspace_context_toggle_floating_uses_selected_floating_container_like_sway() {
    let mut layout = check_ops([
        Op::AddOutput(1),
        Op::AddWindow {
            params: TestWindowParams::new(1),
        },
        Op::ToggleWindowFloating { id: None },
        Op::SplitVertical,
        Op::FocusParent,
        Op::FocusParent,
    ]);

    {
        let workspace = layout.active_workspace().expect("active workspace");
        assert!(workspace.floating_is_active());
        assert!(workspace.debug_floating_workspace_context());
        assert_eq!(workspace.debug_command_context(), "workspace");
        assert_eq!(
            workspace.debug_active_floating_command_container_path(),
            Some(Vec::new()),
            "precondition: workspace context should still retain floating wrapper selection",
        );
    }

    layout.toggle_window_floating(None);

    let workspace = layout.active_workspace().expect("active workspace");
    assert!(
        !workspace.floating_is_active(),
        "workspace-context toggle_floating should restore the selected floating container to tiling",
    );
    assert_eq!(workspace.floating().tiles().count(), 0);
    assert_eq!(workspace.tiling().tiles().count(), 1);
}
#[test]
fn focus_stack_head_is_workspace_in_floating_workspace_context() {
    let mut layout = check_ops([
        Op::AddOutput(1),
        Op::AddWindow {
            params: TestWindowParams::new(1),
        },
        Op::ToggleWindowFloating { id: None },
        Op::FocusParent,
    ]);

    let snapshot = layout.seat_focus.snapshot();
    assert!(
        matches!(snapshot.first(), Some(SeatFocusNode::Workspace { .. })),
        "workspace-context focus must record Workspace at seat-focus head",
    );
    assert!(
        snapshot.iter().any(
            |node| matches!(node, SeatFocusNode::Floating { window_id, .. } if *window_id == 1)
        ),
        "floating node should remain in inactive MRU history",
    );

    layout.switch_focus_floating_tiling();
    let snapshot = layout.seat_focus.snapshot();
    assert!(
        matches!(snapshot.first(), Some(SeatFocusNode::Floating { window_id, .. }) if *window_id == 1),
        "switching back to floating target should restore Floating at seat-focus head",
    );
}
#[test]
fn floating_workspace_context_layout_all_preserves_context_like_split_toggle() {
    let mut layout = check_ops([
        Op::AddOutput(1),
        Op::AddWindow {
            params: TestWindowParams::new(1),
        },
        Op::ToggleWindowFloating { id: None },
        Op::SplitVertical,
        Op::FocusParent,
        Op::FocusParent,
    ]);

    {
        let workspace = layout.active_workspace().expect("active workspace");
        assert!(workspace.debug_floating_workspace_context());
        assert_eq!(workspace.debug_command_context(), "workspace");
    }

    layout.toggle_layout_all();

    let workspace = layout.active_workspace().expect("active workspace");
    assert!(
        workspace.debug_floating_workspace_context(),
        "layout toggle all must preserve the floating workspace context and route to the \
         selected floating container, matching layout toggle split",
    );
}
