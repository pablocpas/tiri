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
