use insta::assert_snapshot;
use proptest::prelude::*;

use super::super::container::{
    ContainerData, ContainerTree, Direction, Layout as ContainerLayout, TreeCommandTarget,
};
use super::super::tile::Tile;
use super::*;

#[test]
fn removing_window_above_preserves_focused_window() {
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
        Op::SetLayoutSplitV,
    ]);
    // Focus middle window and remove the window above it.
    check_ops_on_layout(&mut layout, [Op::FocusWindow(2)]);
    let workspace = layout.active_workspace().expect("active workspace");
    assert_eq!(workspace.tiling().focused_window_id(), Some(2));
    check_ops_on_layout(&mut layout, [Op::CloseWindow(1)]);
    let workspace = layout.active_workspace().expect("active workspace");
    assert_eq!(
        workspace.tiling().focused_window_id(),
        Some(2),
        "removing the window above should not move focus",
    );
}
pub(super) struct TreeHarness {
    pub(super) tree: ContainerTree<TestWindow>,
    options: Rc<Options>,
    clock: Clock,
    view_size: Size<f64, Logical>,
    scale: f64,
}
impl TreeHarness {
    pub(super) fn new() -> Self {
        let options = Rc::new(Options::from_config(&Config::default()));
        let clock = Clock::with_time(Duration::ZERO);
        let view_size = Size::from((800.0, 600.0));
        let working_area = Rectangle::from_size(view_size);
        let scale = 1.0;
        let tree = ContainerTree::new(view_size, working_area, scale, options.clone());
        Self {
            tree,
            options,
            clock,
            view_size,
            scale,
        }
    }

    pub(super) fn add_window(&mut self, id: usize) {
        self.add_window_with_params(TestWindowParams::new(id));
    }

    pub(super) fn add_window_with_params(&mut self, params: TestWindowParams) {
        let window = TestWindow::new(params);
        let tile = Tile::new(
            window,
            self.view_size,
            self.scale,
            self.clock.clone(),
            self.options.clone(),
        );
        self.tree.insert_window(tile);
    }

    pub(super) fn append_window(&mut self, id: usize) {
        self.append_window_with_params(TestWindowParams::new(id));
    }

    pub(super) fn append_window_with_params(&mut self, params: TestWindowParams) {
        let window = TestWindow::new(params);
        let tile = Tile::new(
            window,
            self.view_size,
            self.scale,
            self.clock.clone(),
            self.options.clone(),
        );
        self.tree.append_leaf(tile, true);
    }

    /// Settle the tree the way the spaces do — every mutation there goes through
    /// `mutate_tree`, which relayouts — and then assert it is structurally sound.
    ///
    /// Called automatically on drop, so every test gets an end-state check even though it
    /// drives the tree directly.
    pub(super) fn verify(&mut self) {
        self.tree.layout();
        self.tree.verify_invariants();
    }
}

impl Drop for TreeHarness {
    fn drop(&mut self) {
        // Don't mask the real failure if the test is already unwinding.
        if !std::thread::panicking() {
            self.verify();
        }
    }
}

#[test]
fn moving_a_leaf_between_workspace_stores_keeps_its_identity() {
    let mut source = TreeHarness::new();
    let mut target = TreeHarness::new();
    source.add_window(1);

    let key = source.tree.window_key(&1).expect("source leaf");
    let tile = source.tree.remove_window(&1).expect("detached leaf");
    assert_eq!(tile.node_key(), key);

    target.tree.append_leaf(tile, true);
    assert_eq!(target.tree.window_key(&1), Some(key));
}

#[test]
fn removing_fullscreen_leaf_clears_workspace_pointer() {
    let mut harness = TreeHarness::new();
    harness.add_window(1);

    let key = harness.tree.window_key(&1).expect("fullscreen leaf");
    assert!(harness.tree.set_fullscreen_key(Some(key)));
    assert_eq!(harness.tree.fullscreen_key(), Some(key));

    harness.tree.remove_window(&1).expect("removed leaf");
    assert_eq!(
        harness.tree.fullscreen_key(),
        None,
        "removing the owning leaf must not leave a stale workspace pointer"
    );
}

#[test]
fn extracting_parent_of_fullscreen_leaf_clears_workspace_pointer() {
    let mut harness = TreeHarness::new();
    harness.add_window(1);
    harness.add_window(2);

    let root = harness.tree.root_node_key().expect("workspace root");
    let key = harness.tree.window_key(&1).expect("fullscreen leaf");
    let wrapper = harness
        .tree
        .wrap_child_in_new_container(root, key, ContainerData::new(ContainerLayout::SplitV))
        .expect("wrapper");
    assert!(harness.tree.set_fullscreen_key(Some(key)));

    harness
        .tree
        .take_subtree_at(wrapper)
        .expect("detached fullscreen subtree");
    assert_eq!(
        harness.tree.fullscreen_key(),
        None,
        "recursive extraction must clear a fullscreen descendant"
    );
}

#[test]
fn wrapping_fullscreen_leaf_transfers_workspace_pointer_to_wrapper() {
    let mut harness = TreeHarness::new();
    harness.add_window(1);

    let root = harness.tree.root_node_key().expect("workspace root");
    let leaf = harness.tree.window_key(&1).expect("fullscreen leaf");
    assert!(harness.tree.set_fullscreen_key(Some(leaf)));

    let wrapper = harness
        .tree
        .wrap_child_in_new_container(root, leaf, ContainerData::new(ContainerLayout::SplitV))
        .expect("wrapper");

    assert_eq!(harness.tree.fullscreen_key(), Some(wrapper));
    assert_eq!(harness.tree.fullscreen_representative_window_id(), Some(&1));
    assert_eq!(harness.tree.fullscreen_leaf_window_id(), None);
}

#[test]
fn reaping_empty_fullscreen_wrapper_clears_workspace_pointer() {
    let mut harness = TreeHarness::new();
    harness.add_window(1);

    let root = harness.tree.root_node_key().expect("workspace root");
    let leaf = harness.tree.window_key(&1).expect("leaf");
    let wrapper = harness
        .tree
        .wrap_child_in_new_container(root, leaf, ContainerData::new(ContainerLayout::SplitV))
        .expect("wrapper");
    assert!(harness.tree.set_fullscreen_key(Some(wrapper)));

    harness.tree.remove_window(&1).expect("removed leaf");

    assert_eq!(harness.tree.fullscreen_key(), None);
}

#[test]
fn moving_a_leaf_between_workspace_stores_carries_its_size_state() {
    let mut source = TreeHarness::new();
    let mut target = TreeHarness::new();
    for id in 1..=3 {
        source.add_window(id);
        target.add_window(id + 10);
    }

    let source_root = source.tree.root_node_key().expect("source workspace");
    assert!(source
        .tree
        .set_child_percent(source_root, 1, ContainerLayout::SplitH, 0.6));
    source.tree.layout();

    let key = source.tree.window_key(&2).expect("source leaf");
    let before = source
        .tree
        .debug_node_sizing(key)
        .expect("size state on source leaf");
    assert!(
        before.0 > 0.5,
        "the test needs a distinctive width fraction"
    );
    assert!(before.2 > 0.0, "arrange must record the resize denominator");

    let tile = source.tree.remove_window(&2).expect("detached leaf");
    target.tree.append_leaf(tile, true);

    assert_eq!(target.tree.window_key(&2), Some(key));
    assert_eq!(
        target.tree.debug_node_sizing(key),
        Some(before),
        "identity, both fractions and both child totals travel as one node"
    );

    let target_size = Size::from((1000.0, 700.0));
    target
        .tree
        .set_view_size(target_size, Rectangle::from_size(target_size));
    target.tree.layout();
    let after_arrange = target.tree.debug_node_sizing(key).unwrap();
    assert_ne!(
        after_arrange.2, before.2,
        "the destination arrange replaces the carried denominator with its own span"
    );
}

#[test]
fn moving_a_subtree_between_workspace_stores_keeps_every_identity() {
    let mut source = TreeHarness::new();
    let mut target = TreeHarness::new();
    source.add_window(1);

    let first = source.tree.window_key(&1).expect("first leaf");
    let container = source
        .tree
        .wrap_child_in_new_container(
            source.tree.workspace_root(),
            first,
            ContainerData::new(ContainerLayout::SplitV),
        )
        .expect("moved container");

    let idx = source.tree.focused_root_index().expect("root child");
    let subtree = source
        .tree
        .take_root_child_subtree(idx)
        .expect("detached subtree");
    target.tree.insert_subtree_at_root(0, subtree, true);

    assert_eq!(target.tree.window_key(&1), Some(first));
    assert_eq!(target.tree.parent_of(first), Some(container));
}

#[test]
fn replacing_a_container_hands_its_parent_share_to_the_replacement() {
    let mut harness = TreeHarness::new();
    harness.add_window(1);
    harness.add_window(2);

    let root = harness.tree.workspace_root();
    let leaf = harness.tree.window_key(&1).expect("first leaf");
    let inner = harness
        .tree
        .wrap_child_in_new_container(root, leaf, ContainerData::new(ContainerLayout::SplitV))
        .expect("inner wrapper");
    let outer = harness
        .tree
        .wrap_child_in_new_container(root, inner, ContainerData::new(ContainerLayout::SplitH))
        .expect("outer wrapper");
    assert!(harness
        .tree
        .set_child_percent(outer, 0, ContainerLayout::SplitH, 1.0));

    let replacement_share = harness.tree.debug_node_sizing(inner).unwrap();
    let old_leaf_share = harness.tree.debug_node_sizing(leaf).unwrap();
    assert_ne!(replacement_share.0, old_leaf_share.0);
    assert!(harness.tree.set_fullscreen_key(Some(inner)));

    // `cmd_layout` flattens the inner wrapper with `container_replace` before discovering
    // that the surviving outer wrapper already has the requested layout. There is therefore
    // no arrange pass to hide a missing fraction transfer.
    assert!(!harness
        .tree
        .set_layout_for_target(ContainerLayout::SplitH, TreeCommandTarget::Leaf(leaf),));
    assert_eq!(harness.tree.parent_of(leaf), Some(outer));
    let leaf_share = harness.tree.debug_node_sizing(leaf).unwrap();
    assert_eq!(
        (leaf_share.0, leaf_share.1),
        (replacement_share.0, replacement_share.1),
    );
    assert_eq!(harness.tree.fullscreen_key(), Some(leaf));
    assert!(harness
        .tree
        .get_tile(leaf)
        .expect("replacement leaf")
        .window()
        .pending_sizing_mode()
        .is_fullscreen());
}
#[derive(Debug, Clone, Copy)]
enum TreeRandomOp {
    AddWindow,
    RemoveFocused,
    SplitH,
    SplitV,
    SetTabbed,
    SetStacked,
    ToggleSplit,
    FocusLeft,
    FocusRight,
    FocusUp,
    FocusDown,
    MoveLeft,
    MoveRight,
    MoveUp,
    MoveDown,
    FocusParent,
    FocusChild,
}
/// Apply one fuzz op and assert the tree is still sound, so a corruption is reported at
/// the step that caused it rather than at the end of the sequence.
fn apply_tree_random_op(harness: &mut TreeHarness, op: TreeRandomOp, next_window_id: &mut usize) {
    apply_tree_random_op_inner(harness, op, next_window_id);
    harness.verify();
}

fn apply_tree_random_op_inner(
    harness: &mut TreeHarness,
    op: TreeRandomOp,
    next_window_id: &mut usize,
) {
    use super::super::container::Direction;

    match op {
        TreeRandomOp::AddWindow => {
            harness.add_window(*next_window_id);
            *next_window_id += 1;
        }
        TreeRandomOp::RemoveFocused => {
            if let Some(id) = harness.tree.focused_window_id() {
                let _ = harness.tree.remove_window(&id);
            }
        }
        TreeRandomOp::SplitH => {
            harness.tree.split_focused(ContainerLayout::SplitH);
        }
        TreeRandomOp::SplitV => {
            harness.tree.split_focused(ContainerLayout::SplitV);
        }
        TreeRandomOp::SetTabbed => {
            harness.tree.set_focused_layout(ContainerLayout::Tabbed);
        }
        TreeRandomOp::SetStacked => {
            harness.tree.set_focused_layout(ContainerLayout::Stacked);
        }
        TreeRandomOp::ToggleSplit => {
            harness.tree.toggle_split_layout();
        }
        TreeRandomOp::FocusLeft => {
            harness.tree.focus_in_direction(Direction::Left);
        }
        TreeRandomOp::FocusRight => {
            harness.tree.focus_in_direction(Direction::Right);
        }
        TreeRandomOp::FocusUp => {
            harness.tree.focus_in_direction(Direction::Up);
        }
        TreeRandomOp::FocusDown => {
            harness.tree.focus_in_direction(Direction::Down);
        }
        TreeRandomOp::MoveLeft => {
            harness.tree.move_in_direction(Direction::Left);
        }
        TreeRandomOp::MoveRight => {
            harness.tree.move_in_direction(Direction::Right);
        }
        TreeRandomOp::MoveUp => {
            harness.tree.move_in_direction(Direction::Up);
        }
        TreeRandomOp::MoveDown => {
            harness.tree.move_in_direction(Direction::Down);
        }
        TreeRandomOp::FocusParent => {
            harness.tree.focus_parent();
        }
        TreeRandomOp::FocusChild => {
            harness.tree.focus_child();
        }
    }
}
proptest! {
    #![proptest_config(ProptestConfig {
        cases: 64,
        ..ProptestConfig::default()
    })]

    #[test]
    fn random_container_tree_ops_keep_unique_ids_and_valid_focus(
        ops in prop::collection::vec(
            prop_oneof![
                Just(TreeRandomOp::AddWindow),
                Just(TreeRandomOp::RemoveFocused),
                Just(TreeRandomOp::SplitH),
                Just(TreeRandomOp::SplitV),
                Just(TreeRandomOp::SetTabbed),
                Just(TreeRandomOp::SetStacked),
                Just(TreeRandomOp::ToggleSplit),
                Just(TreeRandomOp::FocusLeft),
                Just(TreeRandomOp::FocusRight),
                Just(TreeRandomOp::FocusUp),
                Just(TreeRandomOp::FocusDown),
                Just(TreeRandomOp::MoveLeft),
                Just(TreeRandomOp::MoveRight),
                Just(TreeRandomOp::MoveUp),
                Just(TreeRandomOp::MoveDown),
                Just(TreeRandomOp::FocusParent),
                Just(TreeRandomOp::FocusChild),
            ],
            1..100
        ),
    ) {
        let mut harness = TreeHarness::new();
        let mut next_window_id = 1usize;

        harness.add_window(next_window_id);
        next_window_id += 1;

        for op in ops {
            apply_tree_random_op(&mut harness, op, &mut next_window_id);

            let tree = harness.tree.debug_tree();
            let ids = harness.tree.all_window_ids();
            let unique = ids.iter().copied().collect::<std::collections::HashSet<_>>();

            prop_assert_eq!(
                ids.len(),
                unique.len(),
                "duplicate window ids after {:?}:\n{}",
                op,
                tree,
            );

            prop_assert_eq!(
                harness.tree.focused_window_id().is_some(),
                !ids.is_empty(),
                "a tree should have a focused window exactly when it is non-empty, after {:?}:\n{}",
                op,
                tree,
            );
        }
    }
}
#[test]
fn move_right_enters_container_with_different_layout() {
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
fn move_right_escapes_to_grandparent_on_layout_mismatch() {
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
fn focus_descends_into_last_focused_child() {
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
        Op::FocusWindow(3),
        Op::FocusColumnRight,
        Op::FocusColumnLeft,
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
fn preserve_explicit_same_layout_container_on_cleanup() {
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
        Op::AddWindow {
            params: TestWindowParams::new(4),
        },
        Op::FocusColumnRight,
        Op::SetLayoutSplitV,
        Op::CloseWindow(3),
    ]);

    let workspace = layout.active_workspace().expect("active workspace");
    let tree = workspace.tiling().debug_tree();
    assert_snapshot!(
        tree.as_str(),
        @"
    SplitH
      SplitV
        SplitV
          Window 1
          Window 4
        Window 2 *
    "
    );
}
#[test]
fn a_container_leaves_nothing_behind_when_its_last_window_closes() {
    // Recorded from sway (tiri-parity/fixtures/layout-outlives-the-window.parity): the
    // tabbed container dies with the window it held, and the workspace is left as it was —
    // splith, with the next window a plain child of it. The layout is not remembered.
    let layout = check_ops([
        Op::AddOutput(1),
        Op::AddWindow {
            params: TestWindowParams::new(1),
        },
        Op::SetLayoutTabbed,
        Op::CloseWindow(1),
        Op::AddWindow {
            params: TestWindowParams::new(2),
        },
    ]);

    let workspace = layout.active_workspace().expect("active workspace");
    assert_eq!(workspace.debug_workspace_layout(), ContainerLayout::SplitH);
    assert_snapshot!(
        workspace.tiling().debug_tree().as_str(),
        @r"
    SplitH
      Window 2 *
    "
    );
}
#[test]
fn cleanup_preserves_single_explicit_split_for_future_inserts() {
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
        Op::CloseWindow(3),
        Op::AddWindow {
            params: TestWindowParams::new(4),
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
        Window 4 *
      Window 2
    "
    );
}
#[test]
fn keep_tabbed_container_on_cleanup_with_split_parent() {
    let mut harness = TreeHarness::new();
    harness.add_window(1);
    harness.add_window(2);
    assert!(harness.tree.focus_window_by_id(&2));
    harness.tree.split_focused(ContainerLayout::Tabbed);
    harness.add_window(3);
    harness.add_window(4);
    let _ = harness.tree.remove_window(&4);

    let tree = harness.tree.debug_tree();
    assert!(
        harness.tree.contains_layout(ContainerLayout::Tabbed),
        "tabbed container should be preserved on cleanup:\n{tree}"
    );
}
#[test]
fn keep_stacked_container_on_cleanup_with_split_parent() {
    let mut harness = TreeHarness::new();
    harness.add_window(1);
    harness.add_window(2);
    assert!(harness.tree.set_focused_layout(ContainerLayout::SplitV));
    assert!(harness.tree.focus_window_by_id(&2));
    harness.tree.split_focused(ContainerLayout::Stacked);
    harness.add_window(3);
    harness.add_window(4);
    let _ = harness.tree.remove_window(&4);

    let tree = harness.tree.debug_tree();
    assert!(
        harness.tree.contains_layout(ContainerLayout::Stacked),
        "stacked container should be preserved on cleanup:\n{tree}"
    );
}
#[test]
fn move_left_enters_single_child_container() {
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
        Op::CloseWindow(3),
        Op::FocusWindow(2),
        Op::MoveColumnLeft,
    ]);

    let workspace = layout.active_workspace().expect("active workspace");
    let tree = workspace.tiling().debug_tree();
    assert_snapshot!(
        tree.as_str(),
        @"
    SplitH
      SplitV
        Window 1
        Window 2 *
    "
    );
}
#[test]
fn move_right_swaps_with_sibling_in_same_layout() {
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
        Op::FocusColumnLeft,
        Op::MoveColumnRight,
    ]);

    let workspace = layout.active_workspace().expect("active workspace");
    let tree = workspace.tiling().debug_tree();
    assert_snapshot!(
        tree.as_str(),
        @"
    SplitH
      Window 1
      Window 3
      Window 2 *
    "
    );
}
#[test]
fn move_down_swaps_in_splitv() {
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
        Op::SetLayoutSplitV,
        Op::FocusWindowUp,
        Op::MoveWindowDown,
    ]);

    let workspace = layout.active_workspace().expect("active workspace");
    let tree = workspace.tiling().debug_tree();
    assert_snapshot!(
        tree.as_str(),
        @"
    SplitH
      SplitV
        Window 1
        Window 3
        Window 2 *
    "
    );
}
#[test]
fn move_down_enters_container_with_different_layout() {
    let layout = check_ops([
        Op::AddOutput(1),
        Op::AddWindow {
            params: TestWindowParams::new(1),
        },
        Op::AddWindow {
            params: TestWindowParams::new(2),
        },
        Op::SetLayoutSplitV,
        Op::SplitHorizontal,
        Op::AddWindow {
            params: TestWindowParams::new(3),
        },
        Op::FocusWindowUp,
        Op::MoveWindowDown,
    ]);

    let workspace = layout.active_workspace().expect("active workspace");
    let tree = workspace.tiling().debug_tree();
    assert_snapshot!(
        tree.as_str(),
        @"
    SplitH
      Window 3
      Window 1 *
      Window 2
    "
    );
}
#[test]
fn move_left_enters_container_with_different_layout() {
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
        Op::FocusColumnRight,
        Op::MoveColumnLeft,
    ]);

    let workspace = layout.active_workspace().expect("active workspace");
    let tree = workspace.tiling().debug_tree();
    assert_snapshot!(
        tree.as_str(),
        @"
    SplitH
      SplitV
        Window 1
        Window 3
        Window 2 *
    "
    );
}
#[test]
fn move_up_enters_container_with_different_layout() {
    let layout = check_ops([
        Op::AddOutput(1),
        Op::AddWindow {
            params: TestWindowParams::new(1),
        },
        Op::AddWindow {
            params: TestWindowParams::new(2),
        },
        Op::SetLayoutSplitV,
        Op::FocusWindowUp,
        Op::SplitHorizontal,
        Op::AddWindow {
            params: TestWindowParams::new(3),
        },
        Op::FocusWindowDown,
        Op::MoveWindowUp,
    ]);

    let workspace = layout.active_workspace().expect("active workspace");
    let tree = workspace.tiling().debug_tree();
    assert_snapshot!(
        tree.as_str(),
        @"
    SplitH
      Window 2 *
      Window 3
      Window 1
    "
    );
}
#[test]
fn move_up_escapes_to_grandparent_on_layout_mismatch() {
    let layout = check_ops([
        Op::AddOutput(1),
        Op::AddWindow {
            params: TestWindowParams::new(1),
        },
        Op::AddWindow {
            params: TestWindowParams::new(2),
        },
        Op::SetLayoutSplitV,
        Op::SplitHorizontal,
        Op::AddWindow {
            params: TestWindowParams::new(3),
        },
        Op::FocusColumnLeft,
        Op::MoveWindowUp,
    ]);

    let workspace = layout.active_workspace().expect("active workspace");
    let tree = workspace.tiling().debug_tree();
    assert_snapshot!(
        tree.as_str(),
        @"
    SplitH
      SplitV
        Window 1
        Window 2 *
        SplitH
          Window 3
    "
    );
}
#[test]
fn preserve_single_child_container_with_different_layout() {
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
        Op::CloseWindow(3),
    ]);

    let workspace = layout.active_workspace().expect("active workspace");
    let tree = workspace.tiling().debug_tree();
    assert_snapshot!(
        tree.as_str(),
        @"
    SplitH
      SplitV
        Window 1 *
      Window 2
    "
    );
}
#[test]
fn replace_single_child_container_with_same_layout() {
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
        Op::SetLayoutSplitH,
        Op::CloseWindow(3),
    ]);

    let workspace = layout.active_workspace().expect("active workspace");
    let tree = workspace.tiling().debug_tree();
    assert_snapshot!(
        tree.as_str(),
        @"
    SplitH
      SplitH
        Window 1 *
      Window 2
    "
    );
}
#[test]
fn move_right_enters_tabbed_container() {
    // Recorded from sway (tiri-parity/fixtures/move-sideways-into-a-tabbed.parity): tabs run
    // left to right like any other horizontal container, so a window moving right into one
    // arrives at its left edge — first in the tab order, whichever tab was on top.
    let mut harness = TreeHarness::new();
    harness.add_window(1);
    harness.add_window(2);
    harness.tree.split_focused(ContainerLayout::Tabbed);
    harness.add_window(3);
    assert!(harness.tree.focus_window_by_id(&1));
    assert!(harness.tree.move_in_direction(Direction::Right));

    let tree = harness.tree.debug_tree();
    assert_snapshot!(
        tree.as_str(),
        @"
    SplitH
      Tabbed
        Window 1 *
        Window 2
        Window 3
    "
    );
}
#[test]
fn move_left_swaps_in_tabbed_layout() {
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
        Op::SetLayoutTabbed,
        Op::MoveColumnLeft,
    ]);

    let workspace = layout.active_workspace().expect("active workspace");
    let tree = workspace.tiling().debug_tree();
    assert_snapshot!(
        tree.as_str(),
        @"
    SplitH
      Tabbed
        Window 1
        Window 3 *
        Window 2
    "
    );
}
#[test]
fn split_inside_tabbed_creates_nested_split() {
    let layout = check_ops([
        Op::AddOutput(1),
        Op::AddWindow {
            params: TestWindowParams::new(1),
        },
        Op::AddWindow {
            params: TestWindowParams::new(2),
        },
        Op::SetLayoutTabbed,
        Op::FocusWindow(1),
        Op::SplitHorizontal,
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
      Tabbed
        SplitH
          Window 1
          Window 3 *
        Window 2
    "
    );
}
#[test]
fn direct_tabbed_tiles_use_content_rect_without_tile_tab_offset() {
    let mut harness = TreeHarness::new();
    harness.add_window(1);
    harness.add_window(2);
    assert!(harness.tree.set_focused_layout(ContainerLayout::Tabbed));
    harness.tree.layout();

    let tiles = harness.tree.all_tiles();
    for id in [1usize, 2] {
        let tile = tiles
            .iter()
            .find(|tile| tile.window().id() == &id)
            .expect("tile should exist");
        assert!(
            tile.in_tabbed_context(),
            "window {id} should be in tabbed context"
        );
        assert_eq!(
            tile.tab_bar_offset(),
            0.0,
            "window {id} should not embed tab bar offset in tile geometry"
        );
    }
}
#[test]
fn tabbed_container_marks_urgent_tab() {
    let mut harness = TreeHarness::new();
    let mut urgent = TestWindowParams::new(1);
    urgent.is_urgent = true;
    harness.add_window_with_params(urgent);
    harness.add_window(2);
    assert!(harness.tree.set_focused_layout(ContainerLayout::Tabbed));
    harness.tree.layout();

    let tab_bar = harness
        .tree
        .tab_bar_layouts()
        .into_iter()
        .next()
        .expect("tabbed tree should expose one tab bar");

    let urgent_tabs = tab_bar
        .tabs
        .iter()
        .filter(|tab| tab.is_urgent)
        .map(|tab| tab.title.as_str())
        .collect::<Vec<_>>();
    assert_eq!(urgent_tabs, vec!["Window 1"]);
}
#[test]
fn tabbed_context_propagates_to_nested_split_tiles() {
    let mut harness = TreeHarness::new();
    harness.add_window(1);
    harness.add_window(2);
    assert!(harness.tree.set_focused_layout(ContainerLayout::Tabbed));
    assert!(harness.tree.focus_window_by_id(&1));
    assert!(harness.tree.split_focused(ContainerLayout::SplitV));
    harness.add_window(3);
    harness.tree.layout();

    let tiles = harness.tree.all_tiles();
    for id in [1usize, 2, 3] {
        let in_tabbed_context = tiles
            .iter()
            .find(|tile| tile.window().id() == &id)
            .map(|tile| tile.in_tabbed_context());
        assert_eq!(
            in_tabbed_context,
            Some(true),
            "window {id} should inherit tabbed border context"
        );
    }
}
#[test]
fn split_only_tiles_do_not_use_tabbed_context() {
    let mut harness = TreeHarness::new();
    harness.add_window(1);
    harness.add_window(2);
    harness.add_window(3);
    harness.tree.layout();

    let tiles = harness.tree.all_tiles();
    for id in [1usize, 2, 3] {
        let in_tabbed_context = tiles
            .iter()
            .find(|tile| tile.window().id() == &id)
            .map(|tile| tile.in_tabbed_context());
        assert_eq!(
            in_tabbed_context,
            Some(false),
            "window {id} should not use tabbed border context in split layout"
        );
    }
}
#[test]
fn toggle_split_layout_switches_orientation() {
    let layout = check_ops([
        Op::AddOutput(1),
        Op::AddWindow {
            params: TestWindowParams::new(1),
        },
        Op::AddWindow {
            params: TestWindowParams::new(2),
        },
        Op::ToggleSplitLayout,
    ]);

    let workspace = layout.active_workspace().expect("active workspace");
    let tree = workspace.tiling().debug_tree();
    assert_snapshot!(
        tree.as_str(),
        @"
    SplitH
      SplitV
        Window 1
        Window 2 *
    "
    );
}

#[test]
fn sway_112_layout_flattens_a_doubly_nested_lone_container_once() {
    let layout = check_ops([
        Op::AddOutput(1),
        Op::AddWindow {
            params: TestWindowParams::new(1),
        },
        Op::SetLayoutStacked,
        Op::SplitVertical,
        Op::ToggleSplitLayout,
    ]);

    let workspace = layout.active_workspace().expect("active workspace");
    let tree = workspace.tiling().debug_tree();
    assert_snapshot!(
        tree.as_str(),
        @"
    SplitH
      SplitH
        Window 1 *
    "
    );
}

#[test]
fn layout_axis_changes_keep_width_and_height_fractions_independent() {
    let mut layout = check_ops([
        Op::AddOutput(1),
        Op::AddWindow {
            params: TestWindowParams::new(1),
        },
        Op::AddWindow {
            params: TestWindowParams::new(2),
        },
        Op::ResizeWindowEdge {
            id: None,
            amount: 150,
            direction: Direction::Left,
        },
        Op::CompleteAnimations,
    ]);

    let resized_width = requested_size(&layout, 2).w;
    assert!(resized_width > requested_size(&layout, 1).w);

    check_ops_on_layout(&mut layout, [Op::ToggleLayoutAll, Op::CompleteAnimations]);
    let vertical_first = requested_size(&layout, 1);
    let vertical_second = requested_size(&layout, 2);
    assert!((vertical_first.h - vertical_second.h).abs() <= 1);

    check_ops_on_layout(&mut layout, [Op::SetLayoutSplitH, Op::CompleteAnimations]);
    assert!((requested_size(&layout, 2).w - resized_width).abs() <= 1);
}

#[test]
fn detached_snapshot_does_not_relabel_fractions_after_parent_axis_change() {
    let mut harness = TreeHarness::new();
    for id in 1..=3 {
        harness.add_window(id);
    }

    let root = harness.tree.root_node_key().unwrap();
    assert!(harness
        .tree
        .set_child_percent(root, 1, ContainerLayout::SplitH, 0.6));

    let key = harness.tree.window_key(&2).unwrap();
    let (subtree, info) = harness.tree.take_subtree_at(key).unwrap();
    let info = info.expect("a root child has insertion metadata");

    assert!(harness
        .tree
        .set_root_container_layout(ContainerLayout::SplitV));
    assert!(harness
        .tree
        .insert_subtree_with_parent_info(&info, subtree, true));
    // New children carry an unset fraction until arrange, just as a newly inserted sway
    // container does. The assertion is about the resolved vertical shares, not the
    // half-finished mutation.
    harness.tree.layout();

    for idx in 0..3 {
        assert!(
            (harness.tree.child_percent(root, idx).unwrap() - 1.0 / 3.0).abs() < 0.000_001,
            "the old horizontal snapshot must not become a vertical resize"
        );
    }
}

#[test]
fn split_wrapper_preserves_the_wrapped_windows_parent_share() {
    let mut layout = check_ops([
        Op::AddOutput(1),
        Op::AddWindow {
            params: TestWindowParams::new(1),
        },
        Op::AddWindow {
            params: TestWindowParams::new(2),
        },
        Op::ToggleSplitLayout,
        Op::MoveWindowUp,
        Op::ResizeWindowEdge {
            id: None,
            amount: -100,
            direction: Direction::Down,
        },
        Op::CompleteAnimations,
    ]);

    let resized_height = requested_size(&layout, 2).h;
    assert!(resized_height < requested_size(&layout, 1).h);
    check_ops_on_layout(&mut layout, [Op::SplitHorizontal, Op::CompleteAnimations]);
    assert!((requested_size(&layout, 2).h - resized_height).abs() <= 1);
}

#[test]
fn tabbed_mutations_leave_fractions_unresolved_until_a_split_is_active() {
    let layout = check_ops([
        Op::AddOutput(1),
        Op::AddWindow {
            params: TestWindowParams::new(1),
        },
        Op::AddWindow {
            params: TestWindowParams::new(2),
        },
        Op::ResizeWindowEdge {
            id: None,
            amount: 150,
            direction: Direction::Left,
        },
        Op::SetLayoutTabbed,
        Op::AddWindow {
            params: TestWindowParams::new(3),
        },
        Op::FocusAlongParent {
            forward: false,
            descend: true,
        },
        Op::CloseWindow(2),
        Op::SetLayoutSplitH,
        Op::CompleteAnimations,
    ]);

    assert!((requested_size(&layout, 1).w - requested_size(&layout, 3).w).abs() <= 1);
}

#[test]
fn toggle_layout_all_cycles_through_all_layouts() {
    // Recorded from sway
    // (tiri-parity/fixtures/layout-toggle-all-on-a-workspace-of-windows.parity): the first
    // toggle builds the container, because a window on the workspace has no container to
    // retype and sway will not hand the workspace a layout a window asked for. The cycle
    // then runs inside that container, so four toggles come back to splith one level down
    // rather than to the flat workspace it started from.
    let layout = check_ops([
        Op::AddOutput(1),
        Op::AddWindow {
            params: TestWindowParams::new(1),
        },
        Op::AddWindow {
            params: TestWindowParams::new(2),
        },
        Op::ToggleLayoutAll,
        Op::ToggleLayoutAll,
        Op::ToggleLayoutAll,
        Op::ToggleLayoutAll,
    ]);

    let workspace = layout.active_workspace().expect("active workspace");
    let tree = workspace.tiling().debug_tree();
    assert_snapshot!(
        tree.as_str(),
        @"
    SplitH
      SplitH
        Window 1
        Window 2 *
    "
    );
}
#[test]
fn move_down_swaps_in_stacked_layout() {
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
        Op::SetLayoutStacked,
        Op::FocusWindowUp,
        Op::MoveWindowDown,
    ]);

    let workspace = layout.active_workspace().expect("active workspace");
    let tree = workspace.tiling().debug_tree();
    assert_snapshot!(
        tree.as_str(),
        @"
    SplitH
      Stacked
        Window 1
        Window 3
        Window 2 *
    "
    );
}
#[test]
fn move_up_escapes_tabbed_layout() {
    let mut harness = TreeHarness::new();
    harness.add_window(1);
    harness.add_window(2);
    assert!(harness.tree.set_focused_layout(ContainerLayout::SplitV));
    harness.tree.split_focused(ContainerLayout::Tabbed);
    harness.add_window(3);
    assert!(harness.tree.focus_window_by_id(&2));
    assert!(harness.tree.move_in_direction(Direction::Up));

    let tree = harness.tree.debug_tree();
    assert_snapshot!(
        tree.as_str(),
        @"
    SplitH
      SplitV
        Window 1
        Window 2 *
        Tabbed
          Window 3
    "
    );
}
#[test]
fn move_left_escapes_stacked_layout() {
    let mut harness = TreeHarness::new();
    harness.add_window(1);
    harness.add_window(2);
    harness.tree.split_focused(ContainerLayout::Stacked);
    harness.add_window(3);
    assert!(harness.tree.focus_window_by_id(&2));
    assert!(harness.tree.move_in_direction(Direction::Left));

    let tree = harness.tree.debug_tree();
    assert_snapshot!(
        tree.as_str(),
        @"
    SplitH
      Window 1
      Window 2 *
      Stacked
        Window 3
    "
    );
}
#[test]
fn move_left_at_edge_is_noop() {
    let layout = check_ops([
        Op::AddOutput(1),
        Op::AddWindow {
            params: TestWindowParams::new(1),
        },
        Op::AddWindow {
            params: TestWindowParams::new(2),
        },
        Op::FocusColumnLeft,
        Op::MoveColumnLeft,
    ]);

    let workspace = layout.active_workspace().expect("active workspace");
    let tree = workspace.tiling().debug_tree();
    assert_snapshot!(
        tree.as_str(),
        @"
    SplitH
      Window 1 *
      Window 2
    "
    );
}
#[test]
fn move_up_at_an_edge_crosses_the_workspace() {
    // Recorded from sway (tiri-parity/fixtures/move-at-an-edge.parity): moving towards an
    // edge with nothing beyond it is not a no-op — it crosses the workspace, which flips.
    // The wrapper tiri leaves behind is an open entry in that fixture's ledger.
    let layout = check_ops([
        Op::AddOutput(1),
        Op::AddWindow {
            params: TestWindowParams::new(1),
        },
        Op::AddWindow {
            params: TestWindowParams::new(2),
        },
        Op::SetLayoutSplitV,
        Op::FocusWindowUp,
        Op::MoveWindowUp,
    ]);

    let workspace = layout.active_workspace().expect("active workspace");
    let tree = workspace.tiling().debug_tree();
    assert_snapshot!(
        tree.as_str(),
        @"
    SplitV
      Window 1 *
      Window 2
    "
    );
}
#[test]
fn split_on_empty_workspace_applies_to_next_window() {
    // i3: a split on an empty workspace sets the workspace's orientation. The first window
    // is a plain child of the workspace, so it needs no wrapper of its own; the orientation
    // becomes visible once a second window arrives.
    let layout = check_ops([
        Op::AddOutput(1),
        Op::SplitVertical,
        Op::AddWindow {
            params: TestWindowParams::new(1),
        },
    ]);

    let workspace = layout.active_workspace().expect("active workspace");
    assert_snapshot!(
        workspace.tiling().debug_tree().as_str(),
        @r"
    SplitV
      Window 1 *
    "
    );

    let mut layout = layout;
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
fn split_on_empty_workspace_applies_to_next_window_via_append() {
    // Pins the append insertion path specifically, which materializes the workspace
    // orientation as a preserved wrapper via ensure_root_container. The user-facing
    // rule — first window gets no wrapper — is pinned by the tests above that go
    // through the public API; this one covers the other insertion path.
    let mut harness = TreeHarness::new();
    assert!(harness.tree.split_focused(ContainerLayout::SplitV));
    harness.append_window(1);

    let tree = harness.tree.debug_tree();
    assert_snapshot!(
        tree.as_str(),
        @"
    SplitV
      Window 1 *
    "
    );
}
#[test]
fn layout_persists_after_last_window_closed() {
    // Closing the last window leaves the workspace orientation in place, so the windows
    // opened afterwards are arranged by it.
    let layout = check_ops([
        Op::AddOutput(1),
        Op::SplitVertical,
        Op::AddWindow {
            params: TestWindowParams::new(1),
        },
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
fn layout_persists_after_last_window_closed_via_append() {
    // Pins the append insertion path specifically, which materializes the workspace
    // orientation as a preserved wrapper via ensure_root_container. The user-facing
    // rule — first window gets no wrapper — is pinned by the tests above that go
    // through the public API; this one covers the other insertion path.
    let mut harness = TreeHarness::new();
    assert!(harness.tree.split_focused(ContainerLayout::SplitV));
    harness.append_window(1);
    let _ = harness.tree.remove_window(&1);
    harness.append_window(2);

    let tree = harness.tree.debug_tree();
    assert_snapshot!(
        tree.as_str(),
        @"
    SplitV
      Window 2 *
    "
    );
}
#[test]
fn split_on_single_window_persists_after_close() {
    // Measured against sway 1.11: `split v` on a lone window builds no container, the
    // workspace carries the orientation, and it outlives the window that set it. The
    // replacement window is a plain child of the workspace, not a wrapped one.
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
    ]);

    let workspace = layout.active_workspace().expect("active workspace");
    assert_eq!(workspace.debug_workspace_layout(), ContainerLayout::SplitV);
    let tree = workspace.tiling().debug_tree();
    assert_snapshot!(
        tree.as_str(),
        @r"
    SplitV
      Window 2 *
    "
    );
}
#[test]
fn split_parallel_with_siblings_wraps_focused_leaf_horizontal() {
    let layout = check_ops([
        Op::AddOutput(1),
        Op::AddWindow {
            params: TestWindowParams::new(1),
        },
        Op::AddWindow {
            params: TestWindowParams::new(2),
        },
        Op::FocusWindow(2),
        Op::SplitHorizontal,
    ]);

    let workspace = layout.active_workspace().expect("active workspace");
    let tree = workspace.tiling().debug_tree();
    assert_snapshot!(
        tree.as_str(),
        @"
    SplitH
      Window 1
      SplitH
        Window 2 *
    "
    );
}
#[test]
fn split_parallel_with_siblings_wraps_focused_leaf_vertical() {
    let layout = check_ops([
        Op::AddOutput(1),
        Op::AddWindow {
            params: TestWindowParams::new(1),
        },
        Op::AddWindow {
            params: TestWindowParams::new(2),
        },
        Op::SetLayoutSplitV,
        Op::FocusWindow(2),
        Op::SplitVertical,
    ]);

    let workspace = layout.active_workspace().expect("active workspace");
    let tree = workspace.tiling().debug_tree();
    assert_snapshot!(
        tree.as_str(),
        @"
    SplitH
      SplitV
        Window 1
        SplitV
          Window 2 *
    "
    );
}
#[test]
fn removing_the_last_sibling_keeps_the_workspace_above_the_container() {
    // Recorded from sway (tiri-parity/fixtures/tabbed-visibility.parity): closing down to
    // one window leaves the container in place under the workspace. Promoting it would
    // hand the workspace that container's layout.
    let mut harness = TreeHarness::new();
    harness.add_window(1);
    harness.add_window(2);

    assert!(harness.tree.focus_window_by_id(&1));
    assert!(harness.tree.split_focused(ContainerLayout::Stacked));
    assert!(harness.tree.focus_window_by_id(&2));
    assert!(harness.tree.split_focused(ContainerLayout::SplitH));

    let _ = harness.tree.remove_window(&2);

    let tree = harness.tree.debug_tree();
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
fn move_right_out_of_a_single_child_container_lands_where_it_was() {
    // Same rule as the leftward case above, recorded in the same fixture.
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
        Op::FocusColumn(1),
        Op::SplitVertical,
        Op::AddWindow {
            params: TestWindowParams::new(4),
        },
        Op::CloseWindow(4),
        Op::FocusColumn(1),
        Op::MoveColumnRight,
    ]);
    let workspace = layout.active_workspace().expect("active workspace");
    let tree = workspace.tiling().debug_tree();
    assert_snapshot!(
        tree.as_str(),
        @"
    SplitH
      Window 1 *
      Window 2
      Window 3
    "
    );
}
#[test]
fn move_left_out_of_a_single_child_container_lands_where_it_was() {
    // Recorded from sway (tiri-parity/fixtures/move-out-of-a-single-child-container.parity):
    // leaving a container puts the window beside where that container was, not one step
    // further. The container's own removal afterwards does not shift it.
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
        Op::FocusColumn(2),
        Op::SplitVertical,
        Op::AddWindow {
            params: TestWindowParams::new(4),
        },
        Op::CloseWindow(4),
        Op::FocusWindow(2),
        Op::MoveColumnLeft,
    ]);
    let workspace = layout.active_workspace().expect("active workspace");
    let tree = workspace.tiling().debug_tree();
    assert_snapshot!(
        tree.as_str(),
        @"
    SplitH
      Window 1
      Window 2 *
      Window 3
    "
    );
}
#[test]
fn move_out_of_explicit_parallel_split_preserves_container_for_reentry() {
    let mut layout = check_ops([
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
        Op::AddWindow {
            params: TestWindowParams::new(4),
        },
        Op::SetLayoutSplitH,
        Op::FocusWindow(4),
        Op::MoveColumnRight,
    ]);
    let workspace = layout.active_workspace().expect("active workspace");
    let after_move_out = workspace.tiling().debug_tree();
    assert_snapshot!(
        after_move_out.as_str(),
        @"
    SplitH
      SplitH
        Window 1
        Window 3
      Window 4 *
      Window 2
    "
    );
    check_ops_on_layout(&mut layout, [Op::MoveColumnLeft]);
    let workspace = layout.active_workspace().expect("active workspace");
    let after_move_back = workspace.tiling().debug_tree();
    assert_snapshot!(
        after_move_back.as_str(),
        @"
    SplitH
      SplitH
        Window 1
        Window 3
        Window 4 *
      Window 2
    "
    );
}
#[test]
fn focus_parent_at_root_is_noop() {
    let mut layout = check_ops([
        Op::AddOutput(1),
        Op::AddWindow {
            params: TestWindowParams::new(1),
        },
    ]);
    // Single window at root - focus_parent should return false
    check_ops_on_layout(&mut layout, [Op::FocusParent]);
}
#[test]
fn focus_parent_child_roundtrip_in_nested_splitv() {
    // Based on focus_descends_into_last_focused_child pattern
    let mut layout = check_ops([
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
        Op::FocusWindow(3),
    ]);
    let workspace = layout.active_workspace().expect("active workspace");
    let tree_before = workspace.tiling().debug_tree();
    // Go up to parent (SplitV container)
    check_ops_on_layout(&mut layout, [Op::FocusParent]);
    // Go back down to child (should return to window 3)
    check_ops_on_layout(&mut layout, [Op::FocusChild]);
    let workspace = layout.active_workspace().expect("active workspace");
    let tree_after = workspace.tiling().debug_tree();
    // Tree should be the same (window 3 still focused)
    assert_eq!(tree_before.as_str(), tree_after.as_str());
}
#[test]
fn focus_parent_traverses_hierarchy() {
    // Kept driving the tree: the assertion is a loop over focus_parent until it stops,
    // which is a property of the tree walk itself rather than a command sequence.
    let mut harness = TreeHarness::new();
    harness.add_window(1);
    harness.add_window(2);
    assert!(harness.tree.focus_in_direction(Direction::Left));
    harness.tree.split_focused(ContainerLayout::SplitV);
    harness.add_window(3);
    assert!(harness.tree.focus_window_by_id(&3));

    // Count how many times we can go up
    let mut levels = 0;
    while harness.tree.focus_parent() {
        levels += 1;
        // Safeguard against infinite loop
        if levels > 10 {
            break;
        }
    }

    // We should be able to go up at least once (from window to container)
    assert!(levels >= 1);
}

/// Crossing to the floating side is a move, so the node keeps its key.
///
/// The property the two-tree model could not have. Everything anyone holds about a node —
/// its place in the seat's focus order above all — is keyed by that, so a crossing that
/// rebuilds the node loses it and a crossing that moves it does not. sway gets this free:
/// `container_set_floating` detaches from one of the workspace's lists and attaches to the
/// other, and the container is the same container throughout.
#[test]
fn floating_a_subtree_keeps_its_identity() {
    let mut harness = TreeHarness::new();
    harness.add_window(1);
    harness.add_window(2);

    let key = harness
        .tree
        .window_key(&1)
        .expect("the window should be in the tree");

    assert!(!harness.tree.is_floating(key));
    let box_of_its_own = Rectangle::new(Point::from((100.0, 50.0)), Size::from((300.0, 200.0)));
    let floating_root = harness
        .tree
        .float_as_group(key, box_of_its_own)
        .expect("the window should become a floating group");
    // Arranged before asking: a detach leaves the siblings' fractions raw on purpose, and the
    // arrange pass is what resolves them. Checking in between asks the tree to be settled
    // halfway through a command.
    harness.tree.layout();
    harness.tree.verify_invariants();

    assert!(
        harness.tree.is_floating(key),
        "the node should now be on the floating side",
    );
    assert_eq!(
        harness.tree.window_key(&1),
        Some(key),
        "the same key, because the node was moved and not rebuilt",
    );
    assert_eq!(
        harness.tree.floating_roots().collect::<Vec<_>>(),
        [floating_root]
    );
    assert_eq!(
        harness.tree.floating_area(floating_root),
        Some(box_of_its_own)
    );

    let root = harness.tree.root_node_key().expect("a workspace root");
    assert_eq!(
        harness.tree.unfloat_group(floating_root, root, 0),
        Some(key)
    );
    assert_eq!(
        harness.tree.window_key(&1),
        Some(key),
        "and the same key on the way back",
    );
    assert!(!harness.tree.is_floating(key));
    assert!(harness.tree.floating_roots().next().is_none());
    harness.tree.layout();
    harness.tree.verify_invariants();
}

/// A floating group is laid out in its own rectangle, not the workspace's.
///
/// The capability the second half needs: sway arranges the two sides separately —
/// `arrange_children` for the tiling, `arrange_floating` for the rest — and neither knows
/// about the other. Here that is one pass told which branch and which box.
#[test]
fn a_floating_branch_is_arranged_in_its_own_rectangle() {
    let mut harness = TreeHarness::new();
    harness.add_window(1);
    harness.add_window(2);

    let key = harness
        .tree
        .window_key(&1)
        .expect("window 1 is in the tree");
    let box_of_its_own = Rectangle::new(Point::from((100.0, 50.0)), Size::from((300.0, 200.0)));
    let floating_root = harness
        .tree
        .float_as_group(key, box_of_its_own)
        .expect("the window should become a floating group");
    harness.tree.layout();

    let data = harness
        .tree
        .collect_branch_layout_data(floating_root, box_of_its_own);

    let laid_out = data
        .leaf_layouts
        .iter()
        .find(|info| info.key == key)
        .expect("the floating leaf should have been arranged");
    assert_eq!(
        laid_out.rect, box_of_its_own,
        "a floating group takes the box it was given, not the workspace's",
    );
}

/// The arrange pass lays out both sides, each in its own box.
///
/// `arrange_workspace` does it with two calls that know nothing about each other:
/// `arrange_children` for the tiling list against the workspace's box, `arrange_floating` for
/// the groups against theirs. One pass here, one tree, two rectangles.
#[test]
fn one_pass_arranges_the_tiled_side_and_the_floating_side() {
    let mut harness = TreeHarness::new();
    harness.add_window(1);
    harness.add_window(2);

    let floated_leaf = harness
        .tree
        .window_key(&1)
        .expect("window 1 is in the tree");
    let box_of_its_own = Rectangle::new(Point::from((120.0, 60.0)), Size::from((240.0, 180.0)));
    harness
        .tree
        .float_as_group(floated_leaf, box_of_its_own)
        .expect("the window should become a floating group");
    harness.tree.layout();
    harness.tree.verify_invariants();

    let rect_of = |key| {
        harness
            .tree
            .leaf_layouts()
            .iter()
            .find(|info| info.key == key)
            .map(|info| info.rect)
    };

    assert_eq!(
        rect_of(floated_leaf),
        Some(box_of_its_own),
        "the floating group takes its own box",
    );

    let tiled = harness
        .tree
        .window_key(&2)
        .expect("window 2 is in the tree");
    let tiled_rect = rect_of(tiled).expect("the tiled window should have been arranged too");
    assert_eq!(
        tiled_rect.size,
        harness.tree.layout_area().size,
        "and the window left behind takes the whole workspace, the floating one not being in \
         its way",
    );
}

/// Asking for "all" has to say all of what, now that there are two sides.
#[test]
fn a_branch_answers_for_itself_and_the_leaf_sweep_answers_for_both() {
    let mut harness = TreeHarness::new();
    harness.add_window(1);
    harness.add_window(2);
    harness.add_window(3);

    let floated_leaf = harness
        .tree
        .window_key(&1)
        .expect("window 1 is in the tree");
    let area = Rectangle::new(Point::from((0.0, 0.0)), Size::from((200.0, 200.0)));
    let floating_root = harness
        .tree
        .float_as_group(floated_leaf, area)
        .expect("the window should become a floating group");
    harness.tree.layout();

    let workspace = harness.tree.root_node_key().expect("a workspace root");
    assert_eq!(
        harness.tree.windows_in_branch(workspace).len(),
        2,
        "the tiled side kept the two that stayed",
    );
    assert_eq!(
        harness.tree.windows_in_branch(floating_root).len(),
        1,
        "and the floating group answers for itself",
    );
    assert_eq!(
        harness.tree.dfs_leaf_keys().len(),
        3,
        "while the leaf sweep sees both sides — a floating window it cannot see is a window \
         whose configure gets computed from a tree without it",
    );
}
