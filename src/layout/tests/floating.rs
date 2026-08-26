use super::*;

#[test]
fn floating_roundtrip_keeps_the_window_node_key() {
    let mut layout = check_ops([
        Op::AddOutput(1),
        Op::AddWindow {
            params: TestWindowParams::new(1),
        },
    ]);

    let key = layout
        .active_workspace()
        .expect("active workspace")
        .container_tree()
        .arena()
        .window_key(&1)
        .expect("mapped window");

    check_ops_on_layout(&mut layout, [Op::ToggleWindowFloating { id: Some(1) }]);
    let workspace = layout.active_workspace().expect("active workspace");
    assert!(workspace.is_floating(&1));
    assert_eq!(workspace.container_tree().arena().window_key(&1), Some(key));

    check_ops_on_layout(&mut layout, [Op::ToggleWindowFloating { id: Some(1) }]);
    let workspace = layout.active_workspace().expect("active workspace");
    assert!(!workspace.is_floating(&1));
    assert_eq!(workspace.container_tree().arena().window_key(&1), Some(key));
}

#[test]
fn unfloat_uses_the_container_preserved_in_the_seat_order() {
    let layout = check_ops([
        Op::AddOutput(1),
        Op::AddWindow {
            params: TestWindowParams::new(1),
        },
        Op::ToggleLayoutAll,
        Op::AddWindow {
            params: TestWindowParams::new(2),
        },
        Op::MoveColumnRight,
        Op::SetLayoutTabbed,
        Op::ToggleWindowFloating { id: None },
        Op::ToggleWindowFloating { id: None },
    ]);

    let workspace = layout.active_workspace().expect("active workspace");
    let tree = workspace.container_tree().arena();
    let first = tree.window_key(&1).expect("first window");
    let second = tree.window_key(&2).expect("second window");
    let inner = tree.parent_of(first).expect("vertical wrapper");
    let outer = tree.parent_of(inner).expect("tabbed wrapper");

    assert_eq!(tree.parent_of(second), Some(outer));
    assert_eq!(
        tree.container_info(outer).map(|(layout, _, _)| layout),
        Some(ContainerLayout::Tabbed),
    );
}

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
        Op::MoveColumnLeft,
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
fn auto_add_window_joins_an_inner_split_of_a_floated_workspace() {
    let layout = check_ops([
        Op::AddOutput(1),
        Op::AddWindow {
            params: TestWindowParams::new(1),
        },
        Op::FocusParent,
        Op::ToggleWindowFloating { id: None },
        Op::SplitToggle,
        Op::AddWindow {
            params: TestWindowParams::new(2),
        },
        Op::AddWindow {
            params: TestWindowParams::new(3),
        },
    ]);

    let workspace = layout.active_workspace().expect("active workspace");
    assert!(workspace.is_floating(&1));
    assert!(workspace.is_floating(&2));
    assert!(workspace.is_floating(&3));
    assert_eq!(workspace.floating().tiles().count(), 3);
}

#[test]
fn auto_add_window_does_not_join_a_tiled_container_floated_as_a_group() {
    let layout = check_ops([
        Op::AddOutput(1),
        Op::AddWindow {
            params: TestWindowParams::new(1),
        },
        Op::AddWindow {
            params: TestWindowParams::new(2),
        },
        Op::MoveColumnRight,
        Op::FocusParent,
        Op::SplitHorizontal,
        Op::ToggleWindowFloating { id: None },
        Op::AddWindow {
            params: TestWindowParams::new(3),
        },
    ]);

    let workspace = layout.active_workspace().expect("active workspace");
    assert!(workspace.is_floating(&1));
    assert!(workspace.is_floating(&2));
    assert!(!workspace.is_floating(&3));
    assert_eq!(layout.focus().map(|win| *win.id()), Some(3));
}

#[test]
fn auto_add_window_does_not_join_a_selected_floating_wrapper() {
    let layout = check_ops([
        Op::AddOutput(1),
        Op::AddWindow {
            params: TestWindowParams::new(1),
        },
        Op::ToggleWindowFloating { id: None },
        Op::SplitToggle,
        Op::FocusParent,
        Op::AddWindow {
            params: TestWindowParams::new(2),
        },
    ]);

    let workspace = layout.active_workspace().expect("active workspace");
    assert!(workspace.is_floating(&1));
    assert!(!workspace.is_floating(&2));
    assert_eq!(layout.focus().map(|window| *window.id()), Some(2));
}

#[test]
fn auto_add_window_joins_below_a_selected_inner_floating_container() {
    let layout = check_ops([
        Op::AddOutput(1),
        Op::AddWindow {
            params: TestWindowParams::new(1),
        },
        Op::ToggleWindowFloating { id: None },
        Op::SplitToggle,
        Op::FocusParent,
        Op::SplitVertical,
        Op::AddWindow {
            params: TestWindowParams::new(2),
        },
    ]);

    let workspace = layout.active_workspace().expect("active workspace");
    assert!(workspace.is_floating(&1));
    assert!(workspace.is_floating(&2));
    assert_eq!(layout.focus().map(|window| *window.id()), Some(2));
}

#[test]
fn split_on_lone_floating_window_publishes_the_explicit_container() {
    let layout = check_ops([
        Op::AddOutput(1),
        Op::AddWindow {
            params: TestWindowParams::new(1),
        },
        Op::ToggleWindowFloating { id: None },
        Op::SplitVertical,
    ]);

    let tree = layout.layout_tree();
    let [root] = tree.floating.as_slice() else {
        panic!("one floating root should be published");
    };
    assert_eq!(
        root.floating_root_kind,
        Some(tiri_ipc::LayoutTreeFloatingRootKind::FloatedContainer),
        "an explicit split is a real sway-visible container, not Tiri scaffolding"
    );
    assert_eq!(root.children.len(), 1);
    assert_eq!(root.children[0].window_id, Some(1));
}

#[test]
fn floating_ipc_order_is_the_reverse_of_the_render_stack() {
    let mut one = TestWindowParams::new(1);
    one.is_floating = true;
    let mut two = TestWindowParams::new(2);
    two.is_floating = true;
    let mut layout = check_ops([
        Op::AddOutput(1),
        Op::AddWindow { params: one },
        Op::AddWindow { params: two },
    ]);

    let workspace = layout.active_workspace().expect("active workspace");
    let render_order: Vec<_> = workspace
        .floating()
        .tiles()
        .map(|tile| *tile.window().id())
        .collect();
    assert_eq!(render_order, [2, 1]);

    let ipc_order: Vec<_> = layout
        .layout_tree()
        .floating
        .iter()
        .map(|root| {
            root.window_id
                .or_else(|| root.children.first().and_then(|child| child.window_id))
                .expect("a one-window floating root")
        })
        .collect();
    assert_eq!(ipc_order, [1, 2]);

    check_ops_on_layout(&mut layout, [Op::SplitHorizontal]);
    let kinds: Vec<_> = layout
        .layout_tree()
        .floating
        .iter()
        .map(|root| root.floating_root_kind)
        .collect();
    assert_eq!(
        kinds,
        [
            Some(tiri_ipc::LayoutTreeFloatingRootKind::ImplicitWindowGroup),
            Some(tiri_ipc::LayoutTreeFloatingRootKind::FloatedContainer),
        ]
    );
}

#[test]
fn focus_prev_sibling_wraps_inside_a_floating_group_after_move() {
    let layout = check_ops([
        Op::AddOutput(1),
        Op::AddWindow {
            params: TestWindowParams::new(1),
        },
        Op::ToggleWindowFloating { id: None },
        Op::SplitVertical,
        Op::AddWindow {
            params: TestWindowParams::new(2),
        },
        Op::MoveWindowUp,
        Op::FocusAlongParent {
            forward: false,
            descend: false,
        },
    ]);

    assert_eq!(layout.focus().map(|window| *window.id()), Some(1));
}

#[test]
fn directional_focus_accepts_zero_distance_between_floating_roots() {
    let layout = check_ops([
        Op::AddOutput(1),
        Op::AddWindow {
            params: TestWindowParams::new(1),
        },
        Op::AddWindow {
            params: TestWindowParams::new(2),
        },
        Op::ToggleWindowFloating { id: None },
        Op::AddWindow {
            params: TestWindowParams::new(3),
        },
        Op::MoveColumnRight,
        Op::ToggleWindowFloating { id: None },
        Op::FocusWindowUp,
    ]);

    assert_eq!(layout.focus().map(|window| *window.id()), Some(2));
}

#[test]
fn directional_focus_does_not_leave_a_fullscreen_floating_root() {
    let layout = check_ops([
        Op::AddOutput(1),
        Op::AddWindow {
            params: TestWindowParams::new(1),
        },
        Op::ToggleWindowFloating { id: None },
        Op::AddWindow {
            params: TestWindowParams::new(2),
        },
        Op::ToggleFullscreenFocused,
        Op::ToggleWindowFloating { id: None },
        Op::FocusParent,
        Op::FocusColumnLeft,
    ]);

    assert_eq!(layout.focus().map(|window| *window.id()), Some(2));
}

#[test]
fn closing_the_last_view_in_a_fullscreen_floating_container_reveals_tiling() {
    let layout = check_ops([
        Op::AddOutput(1),
        Op::AddWindow {
            params: TestWindowParams::new(1),
        },
        Op::ToggleFullscreenFocused,
        Op::AddWindow {
            params: TestWindowParams::new(2),
        },
        Op::ToggleWindowFloating { id: None },
        Op::SplitHorizontal,
        Op::CloseFocused,
    ]);

    let tree = layout.layout_tree();
    assert!(tree.floating.is_empty());
    fn find_window(node: &tiri_ipc::LayoutTreeNode, id: u64) -> Option<&tiri_ipc::LayoutTreeNode> {
        (node.window_id == Some(id)).then_some(node).or_else(|| {
            node.children
                .iter()
                .find_map(|child| find_window(child, id))
        })
    }
    let remaining = tree
        .root
        .as_ref()
        .and_then(|root| find_window(root, 2))
        .expect("surviving tiled window");
    let rect = remaining.rect.expect("arranged tiled window");
    assert!(rect.width > 0.0 && rect.height > 0.0);
}

#[test]
fn unfloat_workspace_wrapper_preserves_selected_descendant() {
    let layout = check_ops([
        Op::AddOutput(1),
        Op::AddWindow {
            params: TestWindowParams::new(1),
        },
        Op::MoveColumnRight,
        Op::FocusParent,
        Op::ToggleWindowFloating { id: None },
        Op::SplitHorizontal,
        Op::ToggleWindowFloating { id: None },
    ]);

    let workspace = layout.active_workspace().expect("active workspace");
    let tree = workspace.container_tree().arena();
    let selected = tree.selected_container_key().expect("selected descendant");
    assert_ne!(tree.parent_of(selected), Some(tree.workspace_root()));
}

#[test]
fn resizing_a_fullscreen_floating_root_edits_its_pending_box() {
    let layout = check_ops([
        Op::AddOutput(1),
        Op::AddWindow {
            params: TestWindowParams::new(1),
        },
        Op::AddWindow {
            params: TestWindowParams::new(2),
        },
        Op::FullscreenWindow(2),
        Op::ToggleWindowFloating { id: None },
        Op::SetWindowWidth {
            id: None,
            change: SizeChange::SetFixed(500),
        },
    ]);

    let rect = layout.layout_tree().floating[0]
        .rect
        .expect("floating fullscreen geometry");
    assert_eq!(rect.width, 500.0);
    assert_eq!(rect.x, 390.0);
}

#[test]
fn resizing_a_fullscreen_floating_container_survives_client_commits() {
    let mut layout = check_ops([
        Op::AddOutput(1),
        Op::AddWindow {
            params: TestWindowParams::new(1),
        },
        Op::AddWindow {
            params: TestWindowParams::new(2),
        },
        Op::FullscreenWindow(2),
        Op::ToggleWindowFloating { id: None },
        Op::SplitHorizontal,
        Op::AddWindow {
            params: TestWindowParams::new(3),
        },
        Op::FocusParent,
    ]);
    check_ops_on_layout(
        &mut layout,
        [
            Op::SetWindowHeight {
                id: None,
                change: SizeChange::SetFixed(400),
            },
            Op::Communicate(1),
            Op::Communicate(2),
            Op::Communicate(3),
        ],
    );
    layout.update_render_elements(None);
    assert_eq!(layout.layout_tree().floating[0].rect.unwrap().height, 400.0);
}

#[test]
fn one_view_floating_container_keeps_its_outer_size_after_client_commit() {
    let cycle = vec![
        LayoutCycleEntry::Layout(ContainerLayout::SplitH),
        LayoutCycleEntry::Layout(ContainerLayout::Tabbed),
        LayoutCycleEntry::Layout(ContainerLayout::Stacked),
        LayoutCycleEntry::Layout(ContainerLayout::SplitV),
    ];
    let layout = check_ops([
        Op::AddOutput(1),
        Op::AddWindow {
            params: TestWindowParams::new(1),
        },
        Op::ToggleLayoutCycle { cycle },
        Op::FocusParent,
        Op::ToggleWindowFloating { id: None },
        Op::ResizeWindowEdge {
            id: None,
            amount: 150,
            direction: Direction::Up,
        },
        Op::Communicate(1),
    ]);

    assert_eq!(layout.layout_tree().floating[0].rect.unwrap().height, 690.0);
}

#[test]
fn refloating_a_resized_view_uses_its_natural_size_again() {
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
    let natural = layout.layout_tree().floating[0]
        .rect
        .expect("initial floating geometry")
        .height;

    check_ops_on_layout(
        &mut layout,
        [
            Op::SetWindowHeight {
                id: None,
                change: SizeChange::AdjustFixed(400),
            },
            Op::ToggleWindowFloating { id: None },
            Op::ToggleWindowFloating { id: None },
        ],
    );

    assert_eq!(
        layout.layout_tree().floating[0].rect.unwrap().height,
        natural
    );
}

#[test]
fn directional_move_on_leaf_inside_floating_wrapper_does_not_translate_group() {
    // Recorded sway sequence: open; floating toggle; split toggle; move right. The split
    // leaves raw focus on the window inside the new floating root. sway therefore tries a
    // structural move, reaches the floating root, and accepts the command as a no-op.
    let mut layout = check_ops([
        Op::AddOutput(1),
        Op::AddWindow {
            params: TestWindowParams::new(1),
        },
        Op::ToggleWindowFloating { id: None },
        Op::SplitToggle,
    ]);

    let before = layout
        .active_workspace()
        .expect("active workspace")
        .floating_container_pos(&1)
        .expect("floating group position");

    check_ops_on_layout(&mut layout, [Op::MoveColumnRight]);

    let after = layout
        .active_workspace()
        .expect("active workspace")
        .floating_container_pos(&1)
        .expect("floating group position");
    assert_eq!(
        after, before,
        "the floating root must not move with its child"
    );
}

#[test]
fn directional_move_on_floating_root_translates_group() {
    let mut layout = check_ops([
        Op::AddOutput(1),
        Op::AddWindow {
            params: TestWindowParams::new(1),
        },
        Op::ToggleWindowFloating { id: None },
    ]);

    let before = layout
        .active_workspace()
        .expect("active workspace")
        .floating_container_pos(&1)
        .expect("floating group position");

    check_ops_on_layout(&mut layout, [Op::MoveColumnRight]);

    let after = layout
        .active_workspace()
        .expect("active workspace")
        .floating_container_pos(&1)
        .expect("floating group position");
    assert_eq!(after.x, before.x + 10.0);
    assert_eq!(after.y, before.y);
}

#[test]
fn directional_move_on_selected_floating_wrapper_translates_group() {
    let mut layout = check_ops([
        Op::AddOutput(1),
        Op::AddWindow {
            params: TestWindowParams::new(1),
        },
        Op::ToggleWindowFloating { id: None },
        Op::SplitToggle,
        Op::FocusParent,
    ]);

    let workspace = layout.active_workspace().expect("active workspace");
    assert!(
        workspace.floating().wrapper_selected_for_window(&1),
        "test precondition: the floating root itself must be selected"
    );
    let before = workspace
        .floating_container_pos(&1)
        .expect("floating group position");

    check_ops_on_layout(&mut layout, [Op::MoveColumnRight]);

    let after = layout
        .active_workspace()
        .expect("active workspace")
        .floating_container_pos(&1)
        .expect("floating group position");
    assert_eq!(after.x, before.x + 10.0);
    assert_eq!(after.y, before.y);
}

#[test]
fn directional_move_reorders_children_inside_floating_wrapper() {
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

    let before_pos = layout
        .active_workspace()
        .expect("active workspace")
        .floating_container_pos(&1)
        .expect("floating group position");

    check_ops_on_layout(&mut layout, [Op::MoveWindowUp]);

    let workspace = layout.active_workspace().expect("active workspace");
    let order: Vec<_> = workspace
        .floating()
        .tiles()
        .map(|tile| *tile.window().id())
        .collect();
    assert_eq!(order, [2, 1]);
    assert_eq!(
        workspace.floating_container_pos(&1),
        Some(before_pos),
        "reordering children must not translate their floating root"
    );
}

#[test]
fn horizontal_move_container_actions_reorder_children_inside_floating_wrapper() {
    // The real i3-profile keybind is `move-container-left`, not Layout::move_left directly.
    // Both spellings must reach the same directional dispatcher on the floating side.
    let mut layout = check_ops([
        Op::AddOutput(1),
        Op::AddWindow {
            params: TestWindowParams::new(1),
        },
        Op::ToggleWindowFloating { id: None },
        Op::SplitHorizontal,
        Op::AddWindow {
            params: TestWindowParams::new(2),
        },
    ]);

    let before_pos = layout
        .active_workspace()
        .expect("active workspace")
        .floating_container_pos(&1)
        .expect("floating group position");

    layout.move_container_left();

    let workspace = layout.active_workspace().expect("active workspace");
    let order: Vec<_> = workspace
        .floating()
        .tiles()
        .map(|tile| *tile.window().id())
        .collect();
    assert_eq!(order, [2, 1]);
    assert_eq!(workspace.floating_container_pos(&1), Some(before_pos));

    layout.move_container_right();

    let workspace = layout.active_workspace().expect("active workspace");
    let order: Vec<_> = workspace
        .floating()
        .tiles()
        .map(|tile| *tile.window().id())
        .collect();
    assert_eq!(order, [1, 2]);
    assert_eq!(workspace.floating_container_pos(&1), Some(before_pos));
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
        assert_eq!(workspace.container_tree().tiles().count(), 0);
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
    assert_eq!(workspace.container_tree().tiles().count(), 2);
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
        Size::from((100, 200)),
        "first floating request should use the size the window mapped with, as sway does"
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
fn floating_axis_resize_preserves_the_container_center() {
    let mut layout = check_ops([
        Op::AddOutput(1),
        Op::AddWindow {
            params: TestWindowParams::new(1),
        },
        Op::ToggleWindowFloating { id: None },
    ]);

    let before = layout.layout_tree().floating[0]
        .rect
        .expect("floating root geometry");
    let before_center = (before.x + before.width / 2., before.y + before.height / 2.);
    check_ops_on_layout(
        &mut layout,
        [
            Op::SetWindowWidth {
                id: None,
                change: SizeChange::AdjustFixed(80),
            },
            Op::SetWindowHeight {
                id: None,
                change: SizeChange::AdjustFixed(100),
            },
        ],
    );

    let after = layout.layout_tree().floating[0]
        .rect
        .expect("floating root geometry");
    let after_center = (after.x + after.width / 2., after.y + after.height / 2.);
    assert!((after_center.0 - before_center.0).abs() < 0.001);
    assert!((after_center.1 - before_center.1).abs() < 0.001);
}

#[test]
fn floating_edge_resize_anchors_the_opposite_edge() {
    let mut layout = check_ops([
        Op::AddOutput(1),
        Op::AddWindow {
            params: TestWindowParams::new(1),
        },
        Op::ToggleWindowFloating { id: None },
    ]);

    let before = tile_rect(&layout, 1);
    let before_size = requested_size(&layout, 1);
    check_ops_on_layout(
        &mut layout,
        [Op::ResizeWindowEdge {
            id: None,
            amount: 40,
            direction: Direction::Left,
        }],
    );

    let after_left = tile_rect(&layout, 1);
    let after_left_size = requested_size(&layout, 1);
    assert_eq!(after_left_size.w, before_size.w + 40);
    assert!((after_left.loc.x - (before.loc.x - 40.)).abs() < 0.001);
    assert!(
        (after_left.loc.x + f64::from(after_left_size.w)
            - (before.loc.x + f64::from(before_size.w)))
        .abs()
            < 0.001
    );

    check_ops_on_layout(
        &mut layout,
        [Op::ResizeWindowEdge {
            id: None,
            amount: -20,
            direction: Direction::Right,
        }],
    );

    let after_right = tile_rect(&layout, 1);
    assert_eq!(requested_size(&layout, 1).w, after_left_size.w - 20);
    assert!((after_right.loc.x - after_left.loc.x).abs() < 0.001);
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
    assert_eq!(workspace.container_tree().tiles().count(), 1);
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
fn floating_roundtrip_keeps_selected_container_as_command_target() {
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

    assert_eq!(
        layout.close_window_ids_for_active_selection(),
        vec![2, 3],
        "precondition: the nested tiling container should be selected",
    );

    layout.toggle_window_floating(None);
    layout.toggle_window_floating(None);

    assert_eq!(
        layout.close_window_ids_for_active_selection(),
        vec![2, 3],
        "floating -> tiling must keep the returned container selected like sway",
    );
}

#[test]
fn floating_toggle_selected_tiling_container_roundtrips_as_the_same_node() {
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
        .container_tree()
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
        assert_eq!(workspace.container_tree().tiles().count(), 2);
        assert_eq!(workspace.floating().tiles().count(), 2);
        for id in &selected_ids {
            assert!(
                workspace.is_floating(id),
                "window {id} should move into the floating container during the first toggle",
            );
        }
    }

    check_ops_on_layout(&mut layout, [Op::ToggleWindowFloating { id: None }]);

    let workspace = layout.active_workspace().expect("active workspace");
    let tree_after = workspace.container_tree().debug_tree().replace(" *", "");
    assert!(
        !workspace.floating_is_active(),
        "toggling the selected floating node should restore that same node to tiling",
    );
    assert_eq!(workspace.floating().tiles().count(), 0);
    assert_eq!(workspace.container_tree().tiles().count(), 4);
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
fn floating_toggle_workspace_subtree_returns_all_windows_inside_the_sway_wrapper() {
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

    let selected_ids = layout.close_window_ids_for_active_selection();
    assert_eq!(
        selected_ids,
        vec![1, 2, 3],
        "precondition: focus-parent should target the whole tiling workspace subtree",
    );

    layout.toggle_window_floating(None);

    {
        let workspace = layout.active_workspace().expect("active workspace");
        assert!(workspace.floating_is_active());
        assert_eq!(workspace.container_tree().tiles().count(), 0);
        assert_eq!(workspace.floating().tiles().count(), 3);
        for id in &selected_ids {
            assert!(
                workspace.is_floating(id),
                "window {id} should move into the floating workspace subtree during the first toggle",
            );
        }
    }

    check_ops_on_layout(&mut layout, [Op::ToggleWindowFloating { id: None }]);

    let workspace = layout.active_workspace().expect("active workspace");
    let tree_after = workspace.container_tree().debug_tree().replace(" *", "");
    assert!(
        !workspace.floating_is_active(),
        "unfloating an all-windows workspace subtree should return focus mode to tiling",
    );
    assert_eq!(workspace.floating().tiles().count(), 0);
    assert_eq!(workspace.container_tree().tiles().count(), 3);
    assert_eq!(
        tree_after, "SplitH\n  SplitH\n    Window 1\n    SplitV\n      Window 2\n      Window 3\n",
        "sway moves the workspace wrapper back into tiling instead of dissolving it",
    );
    for id in selected_ids {
        assert!(
            !workspace.is_floating(&id),
            "window {id} should return to tiling after restoring the whole workspace subtree",
        );
    }
}

#[test]
fn floating_workspace_roundtrip_keeps_wrapper_selected_for_layout_toggle() {
    let mut layout = check_ops([
        Op::AddOutput(1),
        Op::AddWindow {
            params: TestWindowParams::new(1),
        },
        Op::AddWindow {
            params: TestWindowParams::new(2),
        },
        Op::FocusParent,
        Op::FocusParent,
        Op::SetLayoutSplitV,
        Op::SetLayoutTabbed,
        Op::ToggleWindowFloating { id: None },
    ]);

    check_ops_on_layout(
        &mut layout,
        [Op::ToggleWindowFloating { id: None }, Op::ToggleSplitLayout],
    );

    // Sway keeps the restored top-level tabbed wrapper selected. A layout command issued
    // from that top-level container wraps it rather than changing its own layout, so the
    // remembered splitv becomes an intermediate wrapper below the splith workspace.
    // Measured in workspace-floating-roundtrip-layout-toggle.parity.
    let workspace = layout.active_workspace().expect("active workspace");
    let tree = workspace.container_tree().debug_tree().replace(" *", "");
    assert!(
        tree.starts_with("SplitH\n  SplitV\n    Tabbed\n"),
        "the restored wrapper must remain selected so layout toggle wraps it like sway:\n{tree}",
    );
}

#[test]
fn floating_workspace_keeps_layout_on_wrapper_and_resets_workspace_to_splith() {
    let layout = check_ops([
        Op::AddOutput(1),
        Op::AddWindow {
            params: TestWindowParams::new(1),
        },
        Op::AddWindow {
            params: TestWindowParams::new(2),
        },
        Op::FocusParent,
        Op::ToggleSplitLayout,
        Op::ToggleLayoutAll,
        Op::ToggleWindowFloating { id: None },
    ]);

    let workspace = layout.active_workspace().expect("active workspace");
    assert_eq!(workspace.debug_workspace_layout(), ContainerLayout::SplitH);
    let tree = layout.layout_tree();
    let floating = tree.floating.first().expect("floating workspace wrapper");
    assert_eq!(floating.layout, Some(tiri_ipc::LayoutTreeLayout::Stacked));
    let rect = floating.rect.expect("floating wrapper pending box");
    assert_eq!((rect.width, rect.height), (640.0, 540.0));
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
        assert_eq!(workspace.container_tree().tiles().count(), 1);
        let tree = workspace.container_tree().debug_tree();
        assert!(
            !workspace.container_tree().has_containers(),
            "floating->tiling roundtrip for a single implicit container should restore a leaf root:\n{tree}",
        );
    }

    // Toggling back to floating should now match sway semantics (no hidden split wrapper in tiling).
    layout.toggle_window_floating(None);
    let workspace = layout.active_workspace().expect("active workspace");
    assert!(workspace.floating_is_active());
    assert_eq!(workspace.floating().tiles().count(), 1);
    assert_eq!(workspace.container_tree().tiles().count(), 0);
}
#[test]
fn workspace_split_selects_the_new_tiling_wrapper_like_sway() {
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
        assert_eq!(workspace.container_tree().tiles().count(), 1);
    }

    layout.split_horizontal();

    let workspace = layout.active_workspace().expect("active workspace");
    assert!(
        !workspace.floating_is_active(),
        "the wrapper created around tiled children becomes the active side",
    );
    assert_eq!(
        workspace.debug_command_target(),
        "tiling_container",
        "workspace split should select the real wrapper node Sway reports as @0",
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
    let tree_after_return = workspace.container_tree().debug_tree().replace(" *", "");
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
    let tree_after_insert = workspace.container_tree().debug_tree().replace(" *", "");
    assert!(
        tree_after_insert.contains("SplitH\n  Window 1\n  SplitH\n    Window 2\n    Window 3")
            || tree_after_insert
                .contains("SplitH\n  Window 1\n  SplitH\n    Window 3\n    Window 2"),
        "new tiling window should insert inside preserved split container:\n{tree_after_insert}"
    );
}

#[test]
fn workspace_split_wrapper_survives_a_floating_roundtrip() {
    let layout = check_ops([
        Op::AddOutput(1),
        Op::AddWindow {
            params: TestWindowParams::new(1),
        },
        Op::FocusParent,
        Op::SplitHorizontal,
        Op::ToggleWindowFloating { id: None },
        Op::ToggleWindowFloating { id: None },
    ]);

    let workspace = layout.active_workspace().expect("active workspace");
    assert!(!workspace.is_floating(&1));
    assert!(
        workspace.container_tree().selected_is_container(),
        "the same explicit split container must remain selected after returning to tiling"
    );
    assert_eq!(
        workspace.container_tree().debug_tree(),
        "SplitH\n  SplitH\n    Window 1 *\n"
    );
}

#[test]
fn opening_tiled_after_workspace_selection_focuses_the_new_window() {
    let layout = check_ops([
        Op::AddOutput(1),
        Op::AddWindow {
            params: TestWindowParams::new(1),
        },
        Op::ToggleWindowFloating { id: None },
        Op::FocusParent,
        Op::AddWindow {
            params: TestWindowParams::new(2),
        },
    ]);

    let workspace = layout.active_workspace().expect("active workspace");
    assert!(workspace.is_floating(&1));
    assert!(!workspace.is_floating(&2));
    assert_eq!(workspace.container_tree().focused_window_id(), Some(2));
    assert!(
        !workspace.container_tree().selected_is_container(),
        "focusing a newly mapped view must clear the previous workspace selection"
    );
}

#[test]
fn focus_prev_between_floating_roots_focuses_and_raises_the_target() {
    let layout = check_ops([
        Op::AddOutput(1),
        Op::AddWindow {
            params: TestWindowParams::new(1),
        },
        Op::ToggleWindowFloating { id: None },
        Op::FocusParent,
        Op::AddWindow {
            params: TestWindowParams::new(2),
        },
        Op::ToggleWindowFloating { id: None },
        Op::FocusAlongParent {
            forward: false,
            descend: true,
        },
    ]);

    let workspace = layout.active_workspace().expect("active workspace");
    assert_eq!(workspace.container_tree().focused_window_id(), Some(1));
    assert_eq!(
        layout
            .layout_tree()
            .floating
            .iter()
            .filter_map(|node| node.window_id)
            .collect::<Vec<_>>(),
        vec![2, 1],
        "Sway publishes floating_nodes bottom-to-top after raising the focused root"
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
        .container_tree()
        .focus_path();
    layout.activate_window(&3);
    let idx3 = layout
        .active_workspace()
        .expect("active workspace")
        .container_tree()
        .focus_path();
    layout.activate_window(&2);
    let idx2 = layout
        .active_workspace()
        .expect("active workspace")
        .container_tree()
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
fn floating_restore_does_not_relabel_resize_after_workspace_axis_change() {
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
        Op::ResizeWindowEdge {
            id: Some(2),
            amount: 150,
            direction: Direction::Left,
        },
        Op::ToggleWindowFloating { id: Some(2) },
        Op::FocusWindow(1),
        Op::FocusParent,
        Op::SetLayoutSplitV,
        Op::ToggleWindowFloating { id: Some(2) },
        Op::CompleteAnimations,
    ]);

    let heights = [1, 2, 3].map(|id| requested_size(&layout, id).h);
    assert!(
        heights
            .windows(2)
            .all(|pair| (pair[0] - pair[1]).abs() <= 1),
        "the detached horizontal resize must not become a vertical resize: {heights:?}"
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
        .container_tree()
        .focus_path();
    layout.activate_window(&4);
    let path4 = layout
        .active_workspace()
        .expect("active workspace")
        .container_tree()
        .focus_path();
    layout.activate_window(&3);
    let path3 = layout
        .active_workspace()
        .expect("active workspace")
        .container_tree()
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
    let tree = workspace.container_tree().debug_tree().replace(" *", "");
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
    let tree = workspace.container_tree().debug_tree().replace(" *", "");
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
fn floating_split_on_selected_workspace_does_not_retarget_wrapper() {
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
        Some(ContainerLayout::SplitV)
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
    assert_eq!(workspace.debug_workspace_layout(), ContainerLayout::Tabbed);
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
        ActivateWindow::Yes,
    );
    layout.add_window(
        TestWindow::new(TestWindowParams::new(2)),
        AddWindowTarget::Auto,
        None,
        None,
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
        assert_eq!(workspace.container_tree().tiles().count(), 0);
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
    assert_eq!(workspace.container_tree().tiles().count(), 0);
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
        assert_eq!(workspace.container_tree().tiles().count(), 1);
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
    assert_eq!(workspace.container_tree().tiles().count(), 1);
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
    assert_eq!(workspace.container_tree().tiles().count(), 1);
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
    assert_eq!(workspace.container_tree().tiles().count(), 3);
}
#[test]
fn floating_toggle_on_selected_workspace_with_only_floating_is_noop_like_sway() {
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
        assert_eq!(workspace.debug_command_target(), "workspace");
    }

    layout.toggle_window_floating(None);

    let workspace = layout.active_workspace().expect("active workspace");
    assert!(
        workspace.floating_is_active(),
        "the selected workspace has no tiled node that floating toggle could move",
    );
    assert_eq!(workspace.floating().tiles().count(), 1);
    assert_eq!(workspace.container_tree().tiles().count(), 0);
}
#[test]
fn layout_focus_history_keeps_workspace_scope_and_workspace_restores_floating_node() {
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
        snapshot
            .iter()
            .all(|node| matches!(node, SeatFocusNode::Workspace { .. })),
        "layout history must not duplicate node focus owned by the workspace",
    );

    layout.switch_focus_floating_tiling();
    assert!(
        layout
            .active_workspace()
            .and_then(|workspace| workspace.active_window())
            .is_some_and(|window| *window.id() == 1),
        "the workspace seat must restore its own floating target",
    );
}

#[test]
fn floating_active_window_is_filtered_from_workspace_seat_mru() {
    let mut layout = check_ops([
        Op::AddOutput(1),
        Op::AddWindow {
            params: TestWindowParams::new(1),
        },
        Op::AddWindow {
            params: TestWindowParams {
                is_floating: true,
                ..TestWindowParams::new(2)
            },
        },
        Op::AddWindow {
            params: TestWindowParams {
                is_floating: true,
                ..TestWindowParams::new(3)
            },
        },
        Op::FocusWindow(2),
        Op::FocusWindow(1),
    ]);

    let workspace = layout.active_workspace().expect("active workspace");
    assert_eq!(
        workspace.floating().active_window().map(LayoutElement::id),
        Some(&2),
        "while tiling has focus, floating must use the same seat's filtered MRU"
    );

    check_ops_on_layout(&mut layout, [Op::CloseWindow(2)]);
    let workspace = layout.active_workspace().expect("active workspace");
    assert_eq!(
        workspace.floating().active_window().map(LayoutElement::id),
        Some(&3),
        "removing the inactive MRU must reveal the next live seat entry, not a stale cache"
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

/// The two sides of a workspace each ask the space whether they are the focused one, and
/// the answers have to differ. They used to be two fields, and merging them into one left
/// the floating side writing the tiled side's answer after it: every tiled tab bar then
/// drew with no focused tab, because as far as it could tell nothing on its side had focus.
///
/// Nothing rendered in the test suite, so this asks the space directly.
#[test]
fn the_two_sides_of_a_workspace_do_not_overwrite_each_other_s_focus() {
    let mut layout = check_ops([
        Op::AddOutput(1),
        Op::AddWindow {
            params: TestWindowParams::new(1),
        },
    ]);
    layout.update_render_elements(None);

    let workspace = layout.active_workspace().expect("active workspace");
    let containers = workspace.container_tree();
    assert!(
        containers.side_is_active(false),
        "the tiled side holds the focus, so it must render as active"
    );
    assert!(
        !containers.side_is_active(true),
        "and the floating side must not"
    );

    check_ops_on_layout(&mut layout, [Op::ToggleWindowFloating { id: Some(1) }]);
    layout.update_render_elements(None);

    let workspace = layout.active_workspace().expect("active workspace");
    let containers = workspace.container_tree();
    assert!(
        containers.side_is_active(true),
        "the window floated, so the floating side holds the focus"
    );
    assert!(
        !containers.side_is_active(false),
        "and the tiled side must not"
    );
}

/// A workspace has one fullscreen pointer, and the floating list is on the same side of it.
///
/// A floating window carries its client fullscreen state across a workspace move. When the
/// destination already has a fullscreen window, the arriving request is revoked instead of
/// leaving two clients pending fullscreen — the same answer `sync_fullscreen_window` gives on
/// the tiled list.
#[test]
fn a_floating_window_arriving_fullscreen_yields_to_the_one_already_there() {
    let mut floating_1 = TestWindowParams::new(1);
    floating_1.is_floating = true;
    let mut floating_2 = TestWindowParams::new(2);
    floating_2.is_floating = true;

    let layout = check_ops([
        Op::AddOutput(1),
        Op::AddWindow { params: floating_1 },
        Op::FocusWorkspaceDown,
        Op::AddWindow { params: floating_2 },
        Op::ToggleFullscreenFocused,
        Op::FocusWorkspaceUp,
        Op::ToggleFullscreenFocused,
        Op::MoveWindowToWorkspace {
            window_id: Some(1),
            workspace_idx: 1,
            focus: true,
        },
    ]);

    let (_, _, workspace) = layout
        .workspaces()
        .find(|(_, _, ws)| ws.has_window(&2))
        .expect("the destination workspace");
    assert!(workspace.has_window(&1), "window 1 moved here");
    assert_eq!(
        workspace.fullscreen_window_ids(),
        vec![2],
        "window 2 keeps the workspace's one fullscreen pointer"
    );
    assert!(
        !workspace
            .windows()
            .find(|win| win.id() == &1)
            .expect("window 1")
            .pending_sizing_mode()
            .is_fullscreen(),
        "and window 1's stale fullscreen request is revoked"
    );
}
