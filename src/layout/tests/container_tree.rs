use insta::assert_snapshot;
use proptest::prelude::*;

use super::super::container::{ContainerTree, Direction, Layout as ContainerLayout};
use super::super::tile::Tile;
use super::*;

#[test]
fn topology_mutation_stays_dirty_until_apply() {
    let mut harness = TreeHarness::new();
    harness.add_window(1);
    harness.add_window(2);
    harness.tree.layout();

    let width_before = harness.tree.leaf_layouts()[0].rect.size.w;
    let _tile = harness.tree.remove_window(&2).unwrap();
    assert!(harness.tree.topology_is_dirty());
    assert_eq!(harness.tree.leaf_layouts()[0].rect.size.w, width_before);

    harness.tree.apply();
    assert!(!harness.tree.topology_is_dirty());
    assert!(harness.tree.leaf_layouts()[0].rect.size.w > width_before);
}

#[test]
fn failed_topology_mutation_does_not_mark_tree_dirty() {
    let mut harness = TreeHarness::new();
    harness.add_window(1);
    harness.tree.layout();

    assert!(harness.tree.remove_window(&99).is_none());
    assert!(!harness.tree.topology_is_dirty());
}

#[test]
fn next_transaction_survives_while_a_commit_is_in_flight() {
    let mut harness = TreeHarness::new();
    harness.add_window(1);

    let first = Transaction::new();
    harness.tree.set_pending_transaction(first.clone());
    harness.tree.apply();
    assert!(harness.tree.has_pending_commit());

    let second = Transaction::new();
    harness.tree.set_pending_transaction(second.clone());
    harness.add_window(2);

    drop(first);
    harness.tree.apply();
    assert!(
        harness.tree.has_pending_commit(),
        "the queued transaction must govern the relayout after the first commit"
    );

    drop(second);
    harness.tree.apply();
    assert!(!harness.tree.has_pending_commit());
}

#[test]
fn removing_window_above_preserves_focused_window() {
    let mut harness = TreeHarness::new();
    harness.add_window(1);
    harness.add_window(2);
    harness.add_window(3);
    assert!(harness.tree.set_focused_layout(ContainerLayout::SplitV));

    // Focus middle window and remove the window above it.
    assert!(harness.tree.focus_window_by_id(&2));
    let before = harness.tree.debug_tree();
    assert!(before.contains("Window 2 *"));

    assert!(harness.remove_window(1));

    let after = harness.tree.debug_tree();
    assert!(after.contains("Window 2 *"));
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

    pub(super) fn remove_window(&mut self, id: usize) -> bool {
        let Some(_tile) = self.tree.remove_window(&id) else {
            return false;
        };
        self.tree.apply();
        true
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
pub(super) fn parse_debug_tree_windows(tree: &str) -> (Vec<usize>, usize, Option<usize>) {
    let mut ids = Vec::new();
    let mut focused_count = 0usize;
    let mut focused_id = None;

    for line in tree.lines() {
        let trimmed = line.trim();
        let Some(rest) = trimmed.strip_prefix("Window ") else {
            continue;
        };

        let is_focused = rest.ends_with('*');
        let id_text = rest.trim_end_matches('*').trim();
        let id = id_text
            .parse::<usize>()
            .expect("window line in debug tree should contain a numeric id");

        ids.push(id);
        if is_focused {
            focused_count += 1;
            focused_id = Some(id);
        }
    }

    (ids, focused_count, focused_id)
}
pub(super) fn count_root_children_in_debug_tree(tree: &str) -> usize {
    tree.lines()
        .filter(|line| line.starts_with("  ") && !line.starts_with("    "))
        .count()
}
fn apply_tree_random_op(harness: &mut TreeHarness, op: TreeRandomOp, next_window_id: &mut usize) {
    use super::super::container::Direction;

    match op {
        TreeRandomOp::AddWindow => {
            harness.add_window(*next_window_id);
            *next_window_id += 1;
        }
        TreeRandomOp::RemoveFocused => {
            let tree = harness.tree.debug_tree();
            let (_, _, focused_id) = parse_debug_tree_windows(&tree);
            if let Some(id) = focused_id {
                let _ = harness.remove_window(id);
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
            let (ids, focused_count, _focused_id) = parse_debug_tree_windows(&tree);
            let unique = ids.iter().copied().collect::<std::collections::HashSet<_>>();

            prop_assert_eq!(
                ids.len(),
                unique.len(),
                "duplicate window ids after {:?}:\n{}",
                op,
                tree,
            );

            if ids.is_empty() {
                prop_assert_eq!(
                    focused_count,
                    0,
                    "empty tree should not have focused windows after {:?}:\n{}",
                    op,
                    tree,
                );
            } else {
                prop_assert_eq!(
                    focused_count,
                    1,
                    "non-empty tree should have exactly one focused window after {:?}:\n{}",
                    op,
                    tree,
                );
            }
        }
    }
}
#[test]
fn move_right_enters_container_with_different_layout() {
    let mut harness = TreeHarness::new();
    harness.add_window(1);
    harness.add_window(2);
    harness.tree.split_focused(ContainerLayout::SplitV);
    harness.add_window(3);
    assert!(harness.tree.focus_in_direction(Direction::Left));
    assert!(harness.tree.move_in_direction(Direction::Right));

    let tree = harness.tree.debug_tree();
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
    let mut harness = TreeHarness::new();
    harness.add_window(1);
    harness.add_window(2);
    assert!(harness.tree.focus_in_direction(Direction::Left));
    harness.tree.split_focused(ContainerLayout::SplitV);
    harness.add_window(3);
    assert!(harness.tree.move_in_direction(Direction::Right));

    let tree = harness.tree.debug_tree();
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
    let mut harness = TreeHarness::new();
    harness.add_window(1);
    harness.add_window(2);
    assert!(harness.tree.focus_in_direction(Direction::Left));
    harness.tree.split_focused(ContainerLayout::SplitV);
    harness.add_window(3);
    assert!(harness.tree.focus_window_by_id(&3));
    assert!(harness.tree.focus_in_direction(Direction::Right));
    assert!(harness.tree.focus_in_direction(Direction::Left));

    let tree = harness.tree.debug_tree();
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
fn focused_inactive_is_focus_head_of_non_focused_container() {
    // Reproduce: SplitH[ SplitV[win1, win3], win2 ], focus on win3.
    let mut harness = TreeHarness::new();
    harness.add_window(1);
    harness.add_window(2);
    assert!(harness.tree.focus_in_direction(Direction::Left));
    harness.tree.split_focused(ContainerLayout::SplitV);
    harness.add_window(3);
    assert_snapshot!(
        harness.tree.debug_tree().as_str(),
        @"
    SplitH
      SplitV
        Window 1
        Window 3 *
      Window 2
    "
    );

    // Focus the top-level sibling. The SplitV container is now unfocused but
    // keeps win3 as its focus head.
    assert!(harness.tree.focus_window_by_id(&2));
    assert_eq!(harness.tree.focus_path(), vec![1]);

    // win2: globally focused leaf (also its parent's focus head) -> `focused`.
    assert!(harness.tree.path_is_parent_focus_head(&[1]));
    // win3: focus head of the non-focused SplitV -> `focused_inactive`. This is
    // the i3/sway case that the flat active-workspace model got wrong.
    assert!(harness.tree.path_is_parent_focus_head(&[0, 1]));
    // win1: not its parent's focus head -> `unfocused`.
    assert!(!harness.tree.path_is_parent_focus_head(&[0, 0]));
}
#[test]
fn preserve_explicit_same_layout_container_on_cleanup() {
    let mut harness = TreeHarness::new();
    harness.add_window(1);
    harness.add_window(2);
    assert!(harness.tree.focus_in_direction(Direction::Left));
    harness.tree.split_focused(ContainerLayout::SplitV);
    harness.add_window(3);
    harness.add_window(4);
    assert!(harness.tree.focus_in_direction(Direction::Right));
    assert!(harness.tree.set_focused_layout(ContainerLayout::SplitV));
    assert!(harness.remove_window(3));

    let tree = harness.tree.debug_tree();
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
    let mut harness = TreeHarness::new();
    harness.add_window(1);
    assert!(harness.tree.set_focused_layout(ContainerLayout::Tabbed));
    assert!(harness.remove_window(1));

    harness.add_window(2);

    let tree = harness.tree.debug_tree();
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
    let mut harness = TreeHarness::new();
    harness.add_window(1);
    harness.add_window(2);
    assert!(harness.tree.focus_in_direction(Direction::Left));
    harness.tree.split_focused(ContainerLayout::SplitV);
    harness.add_window(3);
    assert!(harness.remove_window(3));

    harness.add_window(4);

    let tree = harness.tree.debug_tree();
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
    assert!(harness.remove_window(4));

    let tree = harness.tree.debug_tree();
    assert!(
        tree.contains("Tabbed"),
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
    assert!(harness.remove_window(4));

    let tree = harness.tree.debug_tree();
    assert!(
        tree.contains("Stacked"),
        "stacked container should be preserved on cleanup:\n{tree}"
    );
}
#[test]
fn move_left_enters_single_child_container() {
    let mut harness = TreeHarness::new();
    harness.add_window(1);
    harness.add_window(2);
    assert!(harness.tree.focus_in_direction(Direction::Left));
    harness.tree.split_focused(ContainerLayout::SplitV);
    harness.add_window(3);
    assert!(harness.remove_window(3));
    assert!(harness.tree.focus_window_by_id(&2));
    assert!(harness.tree.move_in_direction(Direction::Left));

    let tree = harness.tree.debug_tree();
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
    let mut harness = TreeHarness::new();
    harness.add_window(1);
    harness.add_window(2);
    harness.add_window(3);
    assert!(harness.tree.focus_in_direction(Direction::Left));
    assert!(harness.tree.move_in_direction(Direction::Right));

    let tree = harness.tree.debug_tree();
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
    let mut harness = TreeHarness::new();
    harness.add_window(1);
    harness.add_window(2);
    harness.add_window(3);
    assert!(harness.tree.set_focused_layout(ContainerLayout::SplitV));
    assert!(harness.tree.focus_in_direction(Direction::Up));
    assert!(harness.tree.move_in_direction(Direction::Down));

    let tree = harness.tree.debug_tree();
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
    let mut harness = TreeHarness::new();
    harness.add_window(1);
    harness.add_window(2);
    assert!(harness.tree.set_focused_layout(ContainerLayout::SplitV));
    harness.tree.split_focused(ContainerLayout::SplitH);
    harness.add_window(3);
    assert!(harness.tree.focus_in_direction(Direction::Up));
    assert!(harness.tree.move_in_direction(Direction::Down));

    let tree = harness.tree.debug_tree();
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
    let mut harness = TreeHarness::new();
    harness.add_window(1);
    harness.add_window(2);
    assert!(harness.tree.focus_in_direction(Direction::Left));
    harness.tree.split_focused(ContainerLayout::SplitV);
    harness.add_window(3);
    assert!(harness.tree.focus_in_direction(Direction::Right));
    assert!(harness.tree.move_in_direction(Direction::Left));

    let tree = harness.tree.debug_tree();
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
    let mut harness = TreeHarness::new();
    harness.add_window(1);
    harness.add_window(2);
    assert!(harness.tree.set_focused_layout(ContainerLayout::SplitV));
    assert!(harness.tree.focus_in_direction(Direction::Up));
    harness.tree.split_focused(ContainerLayout::SplitH);
    harness.add_window(3);
    assert!(harness.tree.focus_in_direction(Direction::Down));
    assert!(harness.tree.move_in_direction(Direction::Up));

    let tree = harness.tree.debug_tree();
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
    let mut harness = TreeHarness::new();
    harness.add_window(1);
    harness.add_window(2);
    assert!(harness.tree.set_focused_layout(ContainerLayout::SplitV));
    harness.tree.split_focused(ContainerLayout::SplitH);
    harness.add_window(3);
    assert!(harness.tree.focus_in_direction(Direction::Left));
    assert!(harness.tree.move_in_direction(Direction::Up));

    let tree = harness.tree.debug_tree();
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
    let mut harness = TreeHarness::new();
    harness.add_window(1);
    harness.add_window(2);
    assert!(harness.tree.focus_in_direction(Direction::Left));
    harness.tree.split_focused(ContainerLayout::SplitV);
    harness.add_window(3);
    assert!(harness.remove_window(3));

    let tree = harness.tree.debug_tree();
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
    let mut harness = TreeHarness::new();
    harness.add_window(1);
    harness.add_window(2);
    assert!(harness.tree.focus_in_direction(Direction::Left));
    harness.tree.split_focused(ContainerLayout::SplitV);
    harness.add_window(3);
    assert!(harness.tree.set_focused_layout(ContainerLayout::SplitH));
    assert!(harness.remove_window(3));

    let tree = harness.tree.debug_tree();
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
    let mut harness = TreeHarness::new();
    harness.add_window(1);
    harness.add_window(2);
    harness.add_window(3);
    assert!(harness.tree.set_focused_layout(ContainerLayout::Tabbed));
    assert!(harness.tree.move_in_direction(Direction::Left));

    let tree = harness.tree.debug_tree();
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
    let mut harness = TreeHarness::new();
    harness.add_window(1);
    harness.add_window(2);
    assert!(harness.tree.set_focused_layout(ContainerLayout::Tabbed));
    assert!(harness.tree.focus_window_by_id(&1));
    assert!(harness.tree.split_focused(ContainerLayout::SplitH));
    harness.add_window(3);

    let tree = harness.tree.debug_tree();
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
fn changing_planned_titlebar_offset_requests_new_committed_size() {
    let mut harness = TreeHarness::new();
    let mut options = (*harness.options).clone();
    options.layout.tab_bar.show_in_split = true;
    options.layout.tab_bar.height = 24.0;
    harness.options = Rc::new(options.clone());
    harness.tree.update_config(
        harness.view_size,
        Rectangle::from_size(harness.view_size),
        harness.scale,
        harness.options.clone(),
    );

    let mut first = TestWindowParams::new(1);
    first.has_ssd = true;
    let mut second = TestWindowParams::new(2);
    second.has_ssd = true;
    harness.add_window_with_params(first);
    harness.add_window_with_params(second);
    harness.tree.layout();

    let tile = harness
        .tree
        .all_tiles()
        .into_iter()
        .find(|tile| tile.window().id() == &1)
        .unwrap();
    let with_titlebar = tile.window().requested_size().unwrap();
    assert!(tile.tab_bar_offset() > 0.0);

    options.layout.tab_bar.show_in_split = false;
    harness.options = Rc::new(options);
    harness.tree.update_config(
        harness.view_size,
        Rectangle::from_size(harness.view_size),
        harness.scale,
        harness.options.clone(),
    );
    harness.tree.layout();

    let tile = harness
        .tree
        .all_tiles()
        .into_iter()
        .find(|tile| tile.window().id() == &1)
        .unwrap();
    let without_titlebar = tile.window().requested_size().unwrap();
    assert_eq!(tile.tab_bar_offset(), 0.0);
    assert!(without_titlebar.h > with_titlebar.h);
}
#[test]
fn toggle_split_layout_switches_orientation() {
    let mut harness = TreeHarness::new();
    harness.add_window(1);
    harness.add_window(2);
    assert!(harness.tree.toggle_split_layout());

    let tree = harness.tree.debug_tree();
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
    let mut harness = TreeHarness::new();
    harness.add_window(1);
    harness.add_window(2);

    assert!(harness.tree.toggle_layout_all());
    assert!(harness.tree.toggle_layout_all());
    assert!(harness.tree.toggle_layout_all());
    assert!(harness.tree.toggle_layout_all());

    let tree = harness.tree.debug_tree();
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
    let mut harness = TreeHarness::new();
    harness.add_window(1);
    harness.add_window(2);
    harness.add_window(3);
    assert!(harness.tree.set_focused_layout(ContainerLayout::Stacked));
    assert!(harness.tree.focus_in_direction(Direction::Up));
    assert!(harness.tree.move_in_direction(Direction::Down));

    let tree = harness.tree.debug_tree();
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
    let mut harness = TreeHarness::new();
    harness.add_window(1);
    harness.add_window(2);
    assert!(harness.tree.focus_in_direction(Direction::Left));
    assert!(!harness.tree.move_in_direction(Direction::Left));

    let tree = harness.tree.debug_tree();
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
    let mut harness = TreeHarness::new();
    harness.add_window(1);
    harness.add_window(2);
    assert!(harness.tree.set_focused_layout(ContainerLayout::SplitV));
    assert!(harness.tree.focus_in_direction(Direction::Up));
    assert!(!harness.tree.move_in_direction(Direction::Up));

    let tree = harness.tree.debug_tree();
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
    let mut harness = TreeHarness::new();
    assert!(harness.tree.split_focused(ContainerLayout::SplitV));
    harness.add_window(1);

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
fn split_on_empty_workspace_applies_to_next_window_via_append() {
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
    let mut harness = TreeHarness::new();
    assert!(harness.tree.split_focused(ContainerLayout::SplitV));
    harness.add_window(1);
    assert!(harness.remove_window(1));
    harness.add_window(2);

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
fn layout_persists_after_last_window_closed_via_append() {
    let mut harness = TreeHarness::new();
    assert!(harness.tree.split_focused(ContainerLayout::SplitV));
    harness.append_window(1);
    assert!(harness.remove_window(1));
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
    let mut harness = TreeHarness::new();
    harness.add_window(1);
    assert!(harness.tree.split_focused(ContainerLayout::SplitV));
    assert!(harness.remove_window(1));
    harness.add_window(2);

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
fn split_parallel_with_siblings_wraps_focused_leaf_horizontal() {
    let mut harness = TreeHarness::new();
    harness.add_window(1);
    harness.add_window(2);
    assert!(harness.tree.focus_window_by_id(&2));
    assert!(harness.tree.split_focused(ContainerLayout::SplitH));

    let tree = harness.tree.debug_tree();
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
    let mut harness = TreeHarness::new();
    harness.add_window(1);
    harness.add_window(2);
    assert!(harness.tree.set_focused_layout(ContainerLayout::SplitV));
    assert!(harness.tree.focus_window_by_id(&2));
    assert!(harness.tree.split_focused(ContainerLayout::SplitV));

    let tree = harness.tree.debug_tree();
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

    assert!(harness.remove_window(2));

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
    assert!(harness.remove_window(4));

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
    assert!(harness.remove_window(4));
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
    let mut harness = TreeHarness::new();
    harness.add_window(1);
    harness.add_window(2);
    assert!(harness.tree.focus_in_direction(Direction::Left));
    assert!(harness.tree.split_focused(ContainerLayout::SplitV));
    harness.add_window(3);
    harness.add_window(4);
    assert!(harness.tree.set_focused_layout(ContainerLayout::SplitH));
    assert!(harness.tree.focus_window_by_id(&4));

    assert!(harness.tree.move_in_direction(Direction::Right));
    let after_move_out = harness.tree.debug_tree();
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

    assert!(harness.tree.move_in_direction(Direction::Left));
    let after_move_back = harness.tree.debug_tree();
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
    let mut harness = TreeHarness::new();
    harness.add_window(1);

    // Single window at root - focus_parent should return false
    assert!(!harness.tree.focus_parent());
}
#[test]
fn focus_parent_child_roundtrip_in_nested_splitv() {
    // Based on focus_descends_into_last_focused_child pattern
    let mut harness = TreeHarness::new();
    harness.add_window(1);
    harness.add_window(2);
    assert!(harness.tree.focus_in_direction(Direction::Left));
    harness.tree.split_focused(ContainerLayout::SplitV);
    harness.add_window(3);
    assert!(harness.tree.focus_window_by_id(&3));

    let tree_before = harness.tree.debug_tree();

    // Go up to parent (SplitV container)
    assert!(harness.tree.focus_parent());

    // Go back down to child (should return to window 3)
    assert!(harness.tree.focus_child());

    let tree_after = harness.tree.debug_tree();

    // Tree should be the same (window 3 still focused)
    assert_eq!(tree_before.as_str(), tree_after.as_str());
}
#[test]
fn focus_parent_traverses_hierarchy() {
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
