//! Autotiling: the dwindle mode, and the cases where it stands down.

use insta::assert_snapshot;

use super::*;

/// Autotiling on, everything else at its default.
fn autotile_options() -> Options {
    autotile_options_with_ratio(1.)
}

fn autotile_options_with_ratio(ratio: f64) -> Options {
    Options {
        layout: tiri_config::Layout {
            autotile: true,
            autotile_ratio: ratio,
            ..Default::default()
        },
        ..Default::default()
    }
}

fn add_windows(count: usize) -> impl Iterator<Item = Op> {
    (1..=count).map(|id| Op::AddWindow {
        params: TestWindowParams::new(id),
    })
}

fn tree(layout: &Layout<TestWindow>) -> String {
    layout
        .active_workspace()
        .expect("active workspace")
        .container_tree()
        .debug_tree()
}

#[test]
fn each_new_window_splits_the_node_it_lands_beside() {
    let layout = check_ops_with_options(
        autotile_options(),
        std::iter::once(Op::AddOutput(1)).chain(add_windows(4)),
    );

    // 1280x720: the lone window is wider than tall, so the second lands beside it. That
    // halves the width, so the third goes below, and so on down the diagonal.
    assert_snapshot!(
        tree(&layout).as_str(),
        @"
    SplitH
      Window 1
      SplitV
        Window 2
        SplitH
          Window 3
          Window 4 *
    "
    );
}

#[test]
fn the_first_window_is_a_plain_child_of_the_workspace() {
    let layout = check_ops_with_options(
        autotile_options(),
        [
            Op::AddOutput(1),
            Op::AddWindow {
                params: TestWindowParams::new(1),
            },
        ],
    );

    // An empty workspace has nothing to sit beside, so the mode has no question to answer
    // and no wrapper appears — the same shape sway gives a first window.
    assert_snapshot!(
        tree(&layout).as_str(),
        @"
    SplitH
      Window 1 *
    "
    );
}

#[test]
fn the_ratio_decides_which_way_a_node_is_split() {
    // 1264x704 after gaps: 1.795 wide. A ratio above that asks for a column instead.
    let layout = check_ops_with_options(
        autotile_options_with_ratio(2.),
        std::iter::once(Op::AddOutput(1)).chain(add_windows(2)),
    );

    assert_snapshot!(
        tree(&layout).as_str(),
        @"
    SplitV
      Window 1
      Window 2 *
    "
    );
}

#[test]
fn a_window_joining_a_switcher_is_not_split_out_of_it() {
    let layout = check_ops_with_options(
        autotile_options(),
        [
            Op::AddOutput(1),
            Op::AddWindow {
                params: TestWindowParams::new(1),
            },
            Op::AddWindow {
                params: TestWindowParams::new(2),
            },
            Op::SetLayoutTabbed,
            Op::AddWindow {
                params: TestWindowParams::new(3),
            },
        ],
    );

    // Under tabs, "beside" already means another tab. Splitting here would take the window
    // straight back out of the switcher the user just asked for.
    assert_snapshot!(
        tree(&layout).as_str(),
        @"
    SplitH
      Tabbed
        Window 1
        Window 2
        Window 3 *
    "
    );
}

#[test]
fn a_selected_container_keeps_the_plain_placement() {
    let layout = check_ops_with_options(
        autotile_options(),
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
            // Aim the insertion at a subtree rather than at a window.
            Op::FocusParent,
            Op::AddWindow {
                params: TestWindowParams::new(4),
            },
        ],
    );

    // `focus parent` is the user saying where the window goes. The mode does not restate the
    // orientation of a subtree that was selected on purpose, so window 4 joins the selected
    // container's siblings exactly as it would with the mode off.
    assert_snapshot!(
        tree(&layout).as_str(),
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
fn the_binding_turns_the_mode_on_and_off() {
    let mut layout = Layout::<TestWindow>::default();
    assert!(!layout.is_autotile());

    check_ops_on_layout(
        &mut layout,
        [
            Op::AddOutput(1),
            Op::ToggleAutotile,
            Op::AddWindow {
                params: TestWindowParams::new(1),
            },
            Op::AddWindow {
                params: TestWindowParams::new(2),
            },
            Op::ToggleAutotile,
            // With the mode back off, this one is a plain sibling of window 2 rather than a
            // split of it.
            Op::AddWindow {
                params: TestWindowParams::new(3),
            },
        ],
    );

    assert!(!layout.is_autotile());
    assert_snapshot!(
        tree(&layout).as_str(),
        @"
    SplitH
      Window 1
      Window 2
      Window 3 *
    "
    );
}

#[test]
fn floating_windows_are_left_alone() {
    let mut floating = TestWindowParams::new(2);
    floating.is_floating = true;

    let layout = check_ops_with_options(
        autotile_options(),
        [
            Op::AddOutput(1),
            Op::AddWindow {
                params: TestWindowParams::new(1),
            },
            Op::AddWindow { params: floating },
        ],
    );

    // A floating group has no row to dwindle into; its nodes carry their own boxes. Window 1
    // is left exactly as it was, unwrapped, and the focus marker is absent because the focus
    // went to the floating window on the other branch.
    assert_snapshot!(
        tree(&layout).as_str(),
        @"
    SplitH
      Window 1
    "
    );
}

// ---------------------------------------------------------------------------------------
// The invariant the mode can actually promise.
//
// Not a binary tree: tabs, `split` and `focus parent` are the user deciding, and the mode
// never overrides a decision. What it promises is that nobody who only opens, closes and
// moves windows ever ends up with a row of three.
// ---------------------------------------------------------------------------------------

/// One line of `debug_tree`: how deep it sits, and what it is.
fn tree_lines(layout: &Layout<TestWindow>) -> Vec<(usize, String)> {
    tree(layout)
        .lines()
        .filter(|line| !line.trim().is_empty())
        .map(|line| {
            let depth = (line.len() - line.trim_start().len()) / 2;
            (depth, line.trim().trim_end_matches(" *").to_string())
        })
        .collect()
}

/// Every split container below the workspace holds exactly two children.
///
/// The workspace itself is exempt: a tree with one window, or one whose root holds a single
/// container, is the ordinary shape and `split none` is defined never to climb through it.
/// Switchers are exempt because a five-tab switcher is a thing someone asked for.
#[track_caller]
fn assert_split_containers_are_pairs(layout: &Layout<TestWindow>) {
    let lines = tree_lines(layout);

    for (idx, (depth, label)) in lines.iter().enumerate() {
        if *depth == 0 || !matches!(label.as_str(), "SplitH" | "SplitV") {
            continue;
        }

        let children = lines[idx + 1..]
            .iter()
            .take_while(|(child_depth, _)| child_depth > depth)
            .filter(|(child_depth, _)| child_depth == &(depth + 1))
            .count();

        assert_eq!(
            children,
            2,
            "`{label}` at depth {depth} holds {children} children, not a pair\n{}",
            tree(layout)
        );
    }
}

fn open_close_move_op() -> impl Strategy<Value = Op> {
    prop_oneof![
        (1..=8usize).prop_map(|id| Op::AddWindow {
            params: TestWindowParams::new(id),
        }),
        Just(Op::CloseFocused),
        Just(Op::MoveColumnLeft),
        Just(Op::MoveColumnRight),
        Just(Op::MoveWindowDown),
        Just(Op::MoveWindowUp),
        Just(Op::FocusColumnLeft),
        Just(Op::FocusColumnRight),
        Just(Op::FocusWindowDown),
        Just(Op::FocusWindowUp),
    ]
}

proptest! {
    #[test]
    fn opening_closing_and_moving_never_builds_a_trio(
        ops in prop::collection::vec(open_close_move_op(), 0..40),
    ) {
        let mut layout =
            Layout::with_options(Clock::with_time(Duration::ZERO), autotile_options());
        Op::AddOutput(1).apply(&mut layout);

        for op in ops {
            op.apply(&mut layout);
            layout.verify_invariants();
            assert_split_containers_are_pairs(&layout);
        }
    }
}
#[test]
fn flattening_a_level_never_widens_the_row_it_sits_in() {
    // i3 dissolves a redundant level by splicing its children into the grandparent, which is
    // how a pair becomes a trio without anyone asking. Under the mode the same level goes,
    // but in place. This is the shape that caught it.
    let layout = check_ops_with_options(
        autotile_options(),
        [
            Op::AddOutput(1),
            Op::AddWindow {
                params: TestWindowParams::new(2),
            },
            Op::AddWindow {
                params: TestWindowParams::new(3),
            },
            Op::MoveWindowUp,
            Op::AddWindow {
                params: TestWindowParams::new(4),
            },
            Op::MoveWindowDown,
            Op::MoveColumnLeft,
            Op::AddWindow {
                params: TestWindowParams::new(1),
            },
            Op::MoveColumnRight,
            Op::FocusWindowDown,
            Op::MoveWindowDown,
        ],
    );

    assert_snapshot!(
        tree(&layout).as_str(),
        @"
    SplitV
      SplitH
        Window 4
        SplitH
          Window 1
          Window 3
      Window 2 *
    "
    );
    assert_split_containers_are_pairs(&layout);
}
