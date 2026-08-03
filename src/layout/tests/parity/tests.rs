//! What tiri does, in the terms sway is measured in.
//!
//! Each test here is a script plus the observable model after every command. The expected
//! text is not a snapshot of whatever tiri happened to do: the scenarios marked *measured*
//! were run against sway 1.11 and written down in `docs/design/parity.md` before tiri was
//! asked the same question. Once the recorder lands (step 3) these become fixtures generated
//! from sway itself; until then they are the strongest ground truth available, and they are
//! what validates the replayer.

use super::replay;

#[track_caller]
fn assert_replay(script: &str, expected: &str) {
    let actual = replay(script).render();
    assert_eq!(
        actual.trim_end(),
        expected.trim_end(),
        "\n--- tiri ---\n{actual}\n--- expected ---\n{expected}"
    );
}

#[test]
fn a_script_that_opens_nothing_still_has_a_workspace() {
    assert_replay(
        "split v",
        "\
$ split v
workspace splitv focus=@
",
    );
}

/// Measured, scenario A: `split v` on an empty workspace creates no container. The
/// orientation lands on the workspace, and windows opened afterwards stack vertically as its
/// direct children.
#[test]
fn split_on_an_empty_workspace_orients_the_workspace() {
    assert_replay(
        "\
split v
open
open
",
        "\
$ split v
workspace splitv focus=@

$ open
workspace splitv focus=1
  window 1 0.000,0.000 1.000x1.000

$ open
workspace splitv focus=2
  window 1 0.000,0.000 1.000x0.500
  window 2 0.000,0.500 1.000x0.500
",
    );
}

/// Measured, scenario B: `split v` on a lone window also creates no container, and the
/// orientation outlives the window that was focused when it was set.
#[test]
fn split_on_a_lone_window_orients_the_workspace_and_outlives_it() {
    assert_replay(
        "\
open
split v
close
open
open
",
        "\
$ open
workspace splith focus=1
  window 1 0.000,0.000 1.000x1.000

$ split v
workspace splitv focus=1
  window 1 0.000,0.000 1.000x1.000

$ close
workspace splitv focus=@

$ open
workspace splitv focus=2
  window 2 0.000,0.000 1.000x1.000

$ open
workspace splitv focus=3
  window 2 0.000,0.000 1.000x0.500
  window 3 0.000,0.500 1.000x0.500
",
    );
}

/// Measured, scenario C: `split v` on a window that has siblings creates the container
/// immediately, holding one child, and sway does not collapse it. The next window opens
/// inside it.
#[test]
fn split_on_a_window_with_siblings_creates_the_container_immediately() {
    assert_replay(
        "\
open
open
split v
open
",
        "\
$ open
workspace splith focus=1
  window 1 0.000,0.000 1.000x1.000

$ open
workspace splith focus=2
  window 1 0.000,0.000 0.500x1.000
  window 2 0.500,0.000 0.500x1.000

$ split v
workspace splith focus=2
  window 1 0.000,0.000 0.500x1.000
  splitv 0.500,0.000 0.500x1.000
    window 2 0.500,0.000 0.500x1.000

$ open
workspace splith focus=3
  window 1 0.000,0.000 0.500x1.000
  splitv 0.500,0.000 0.500x1.000
    window 2 0.500,0.000 0.500x0.500
    window 3 0.500,0.500 0.500x0.500
",
    );
}

/// Measured, scenario D: the workspace is a container with an orientation, so `layout tabbed`
/// on an empty workspace makes the workspace itself tabbed rather than wrapping anything.
#[test]
fn layout_tabbed_on_an_empty_workspace_makes_the_workspace_tabbed() {
    assert_replay(
        "\
layout tabbed
open
open
",
        "\
$ layout tabbed
workspace tabbed focus=@

$ open
workspace tabbed focus=1
  window 1 0.000,0.000 1.000x1.000

$ open
workspace tabbed focus=2
  window 1 0.000,0.000 1.000x1.000 hidden
  window 2 0.000,0.000 1.000x1.000
",
    );
}

/// Under a tabbed container only the selected child is visible, and moving focus moves which
/// one that is. Tab bar height is erased, so the two children share a rectangle.
///
/// The tabbed container sits *under* the workspace, which keeps splith: a layout command
/// issued from a window never hands the workspace a layout.
#[test]
fn focus_moves_which_tab_is_on_top() {
    assert_replay(
        "\
open
open
layout tabbed
focus left
",
        "\
$ open
workspace splith focus=1
  window 1 0.000,0.000 1.000x1.000

$ open
workspace splith focus=2
  window 1 0.000,0.000 0.500x1.000
  window 2 0.500,0.000 0.500x1.000

$ layout tabbed
workspace splith focus=2
  tabbed 0.000,0.000 1.000x1.000
    window 1 0.000,0.000 1.000x1.000 hidden
    window 2 0.000,0.000 1.000x1.000

$ focus left
workspace splith focus=1
  tabbed 0.000,0.000 1.000x1.000
    window 1 0.000,0.000 1.000x1.000
    window 2 0.000,0.000 1.000x1.000 hidden
",
    );
}

#[test]
fn moving_a_window_reorders_it_among_its_siblings() {
    assert_replay(
        "\
open
open
move left
",
        "\
$ open
workspace splith focus=1
  window 1 0.000,0.000 1.000x1.000

$ open
workspace splith focus=2
  window 1 0.000,0.000 0.500x1.000
  window 2 0.500,0.000 0.500x1.000

$ move left
workspace splith focus=2
  window 2 0.000,0.000 0.500x1.000
  window 1 0.500,0.000 0.500x1.000
",
    );
}

/// A lone floating window is the window itself, not tiri's wrapper container, and the tiled
/// window left behind grows into the space.
#[test]
fn a_floating_window_is_reported_as_floating() {
    assert_replay(
        "\
open
open
floating toggle
",
        "\
$ open
workspace splith focus=1
  window 1 0.000,0.000 1.000x1.000

$ open
workspace splith focus=2
  window 1 0.000,0.000 0.500x1.000
  window 2 0.500,0.000 0.500x1.000

$ floating toggle
workspace splith focus=2
  window 1 0.000,0.000 1.000x1.000
  window 2 0.345,0.300 0.309x0.400 floating
",
    );
}

/// Measured: `layout X` on a window whose parent is the workspace cannot hand the layout to
/// the workspace. A container takes the workspace's children instead, and the workspace
/// keeps its own orientation — this is what makes `layout splitv` and `split v` different
/// commands here.
#[test]
fn layout_on_a_workspace_child_builds_a_container_instead() {
    assert_replay(
        "\
open
layout stacking
",
        "\
$ open
workspace splith focus=1
  window 1 0.000,0.000 1.000x1.000

$ layout stacking
workspace splith focus=1
  stacked 0.000,0.000 1.000x1.000
    window 1 0.000,0.000 1.000x1.000
",
    );
}

/// Measured: repeating a layout never nests. The second command targets the container the
/// first one built, which already has that layout.
#[test]
fn repeating_a_layout_command_changes_nothing() {
    let once = replay("open\nlayout tabbed\n").render();
    let thrice = replay("open\nlayout tabbed\nlayout tabbed\nlayout tabbed\n").render();

    let last = |text: &str| text.trim_end().rsplit("$ ").next().unwrap().to_owned();
    assert_eq!(last(&once), last(&thrice));
}

/// Measured: each `layout`/`split` pair inside a tabbed container adds a level, and sway
/// adds them too. tiri used to collapse these into one wrapper.
#[test]
fn splitting_inside_a_tabbed_container_nests_a_level_each_time() {
    assert_replay(
        "\
open
layout tabbed
split v
layout tabbed
",
        "\
$ open
workspace splith focus=1
  window 1 0.000,0.000 1.000x1.000

$ layout tabbed
workspace splith focus=1
  tabbed 0.000,0.000 1.000x1.000
    window 1 0.000,0.000 1.000x1.000

$ split v
workspace splith focus=1
  tabbed 0.000,0.000 1.000x1.000
    splitv 0.000,0.000 1.000x1.000
      window 1 0.000,0.000 1.000x1.000

$ layout tabbed
workspace splith focus=1
  tabbed 0.000,0.000 1.000x1.000
    tabbed 0.000,0.000 1.000x1.000
      window 1 0.000,0.000 1.000x1.000
",
    );
}

// `split X` with the workspace selected is recorded, not written out here: this test used to
// hold the same script with a hand-written expectation, and the hand-written part was wrong
// about which node stays selected. See fixtures/split-on-the-workspace-with-two-windows.parity
// and fixtures/split-with-the-workspace-selected.parity.

#[test]
fn an_unknown_command_fails_the_script_instead_of_being_skipped() {
    let err = std::panic::catch_unwind(|| {
        replay("open\nresize grow width 10 px\n");
    });
    let err = *err.unwrap_err().downcast::<String>().unwrap();
    assert!(
        err.contains("line 2") && err.contains("no Op implements this command"),
        "a command the table does not know must name itself: {err}"
    );
}
