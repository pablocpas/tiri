use insta::assert_snapshot;
use proptest::prelude::*;

use super::super::container::{ContainerTree, Direction, Layout as ContainerLayout};
use super::super::tile::Tile;
use super::*;

#[test]
fn removing_window_above_preserves_focused_window() {
    let mut harness = TreeHarness::new();
    harness.add_window(1);
    harness.add_window(2);
    harness.add_window(3);
    assert!(harness.tree.set_focused_layout(ContainerLayout::SplitV));

    // Focus middle window and remove the window above it.
    assert!(harness.tree.focus_window_by_id(&2));
    assert_eq!(harness.tree.focused_window_id(), Some(2));

    let _ = harness.tree.remove_window(&1);

    assert_eq!(
        harness.tree.focused_window_id(),
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
    SplitV
      SplitV
        Window 1
        Window 4
      Window 2 *
    "
    );
}
#[test]
fn cleanup_reuses_last_root_layout_after_tree_becomes_empty() {
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
    let tree = workspace.tiling().debug_tree();
    assert_snapshot!(
        tree.as_str(),
        @"
    Tabbed
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
    SplitV
      SplitH
        Window 2
        Window 1 *
        Window 3
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
    SplitV
      SplitH
        Window 1
        Window 3
        Window 2 *
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
        Window 2
        Window 3
        Window 1 *
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
    SplitV
      Window 1
      Window 2 *
    "
    );
}
#[test]
fn toggle_layout_all_cycles_through_all_layouts() {
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
fn move_up_at_edge_is_noop() {
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
    assert_snapshot!(workspace.tiling().debug_tree().as_str(), @"Window 1 *");

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
    let tree = workspace.tiling().debug_tree();
    assert_snapshot!(
        tree.as_str(),
        @"
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
    SplitV
      Window 1
      SplitV
        Window 2 *
    "
    );
}
#[test]
fn removing_last_sibling_flattens_non_preserved_root_container() {
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
    Stacked
      Window 1 *
    "
    );
}
#[test]
fn wrap_root_for_sibling_insert_uses_pending_layout_hint() {
    let mut harness = TreeHarness::new();
    harness.add_window(1);
    harness.add_window(2);

    harness.tree.set_pending_layout(ContainerLayout::Tabbed);
    assert!(harness.tree.wrap_root_for_sibling_insert());

    let tree = harness.tree.debug_tree().replace(" *", "");
    assert!(
        tree.contains("Tabbed\n  SplitH\n    Window 1\n    Window 2"),
        "wrapping root for sibling insert should honor pending layout hint:\n{tree}"
    );
}
#[test]
fn move_right_from_single_child_container_is_atomic() {
    let mut harness = TreeHarness::new();
    harness.add_window(1);
    harness.add_window(2);
    harness.add_window(3);

    assert!(harness.tree.focus_root_child(0));
    assert!(harness.tree.split_focused(ContainerLayout::SplitV));
    harness.add_window(4);
    let _ = harness.tree.remove_window(&4);

    assert!(harness.tree.focus_root_child(0));
    assert!(harness.tree.move_in_direction(Direction::Right));

    let tree = harness.tree.debug_tree();
    assert_snapshot!(
        tree.as_str(),
        @"
    SplitH
      Window 2
      Window 1 *
      Window 3
    "
    );
}
#[test]
fn move_left_swaps_single_child_container_immediately() {
    let mut harness = TreeHarness::new();
    harness.add_window(1);
    harness.add_window(2);
    harness.add_window(3);

    assert!(harness.tree.focus_root_child(1));
    assert!(harness.tree.split_focused(ContainerLayout::SplitV));
    harness.add_window(4);
    let _ = harness.tree.remove_window(&4);
    assert!(harness.tree.focus_window_by_id(&2));

    assert!(harness.tree.move_in_direction(Direction::Left));

    let tree = harness.tree.debug_tree();
    assert_snapshot!(
        tree.as_str(),
        @"
    SplitH
      Window 2 *
      Window 1
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
