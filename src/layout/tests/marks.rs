use super::*;

#[test]
fn marks_replace_add_toggle() {
    let options = Options::from_config(&Config::default());
    let mut layout = Layout::with_options(Clock::with_time(Duration::ZERO), options);

    let output = make_test_output("output-test");
    layout.add_output(output.clone(), None);

    let params1 = TestWindowParams::new(1);
    let id1 = params1.id;
    layout.add_window(
        TestWindow::new(params1),
        AddWindowTarget::Auto,
        None,
        None,
        false,
        ActivateWindow::Yes,
    );

    let params2 = TestWindowParams::new(2);
    let id2 = params2.id;
    layout.add_window(
        TestWindow::new(params2),
        AddWindowTarget::Auto,
        None,
        None,
        false,
        ActivateWindow::Yes,
    );

    let workspace = layout.active_workspace_mut().expect("active workspace");
    assert!(workspace.focus_window_by_id(&id1));

    layout.mark_focused(String::from("one"), MarkMode::Replace);
    assert_eq!(marks_for(&layout, id1), vec![String::from("one")]);

    let workspace = layout.active_workspace_mut().expect("active workspace");
    assert!(workspace.focus_window_by_id(&id2));

    layout.mark_focused(String::from("one"), MarkMode::Add);
    assert!(marks_for(&layout, id1).is_empty());
    assert_eq!(marks_for(&layout, id2), vec![String::from("one")]);

    layout.mark_focused(String::from("one"), MarkMode::Toggle);
    assert!(marks_for(&layout, id2).is_empty());
}
#[test]
fn marks_multiple_on_same_window() {
    let options = Options::from_config(&Config::default());
    let mut layout = Layout::with_options(Clock::with_time(Duration::ZERO), options);

    let output = make_test_output("output-test");
    layout.add_output(output.clone(), None);

    let params1 = TestWindowParams::new(1);
    let id1 = params1.id;
    layout.add_window(
        TestWindow::new(params1),
        AddWindowTarget::Auto,
        None,
        None,
        false,
        ActivateWindow::Yes,
    );

    // Add multiple marks to the same window
    layout.mark_focused(String::from("mark_a"), MarkMode::Add);
    layout.mark_focused(String::from("mark_b"), MarkMode::Add);
    layout.mark_focused(String::from("mark_c"), MarkMode::Add);

    let marks = marks_for(&layout, id1);
    assert!(marks.contains(&String::from("mark_a")));
    assert!(marks.contains(&String::from("mark_b")));
    assert!(marks.contains(&String::from("mark_c")));
    assert_eq!(marks.len(), 3);
}
#[test]
fn marks_unique_across_windows() {
    // When using Replace mode, mark moves from old window to new window
    let options = Options::from_config(&Config::default());
    let mut layout = Layout::with_options(Clock::with_time(Duration::ZERO), options);

    let output = make_test_output("output-test");
    layout.add_output(output.clone(), None);

    let params1 = TestWindowParams::new(1);
    let id1 = params1.id;
    layout.add_window(
        TestWindow::new(params1),
        AddWindowTarget::Auto,
        None,
        None,
        false,
        ActivateWindow::Yes,
    );

    let params2 = TestWindowParams::new(2);
    let id2 = params2.id;
    layout.add_window(
        TestWindow::new(params2),
        AddWindowTarget::Auto,
        None,
        None,
        false,
        ActivateWindow::Yes,
    );

    // Add mark to window 1
    let workspace = layout.active_workspace_mut().expect("active workspace");
    assert!(workspace.focus_window_by_id(&id1));
    layout.mark_focused(String::from("unique_mark"), MarkMode::Replace);
    assert_eq!(marks_for(&layout, id1), vec![String::from("unique_mark")]);

    // Focus window 2 and add the same mark - should move from window 1 to window 2
    let workspace = layout.active_workspace_mut().expect("active workspace");
    assert!(workspace.focus_window_by_id(&id2));
    layout.mark_focused(String::from("unique_mark"), MarkMode::Replace);

    // Mark should now be only on window 2, not on window 1
    assert!(marks_for(&layout, id1).is_empty());
    assert_eq!(marks_for(&layout, id2), vec![String::from("unique_mark")]);
}

#[test]
fn a_leaf_mark_survives_cross_workspace_detach_and_attach() {
    let mut layout = check_ops([
        Op::AddOutput(1),
        Op::AddWindow {
            params: TestWindowParams::new(1),
        },
    ]);

    layout.mark_focused(String::from("travels"), MarkMode::Replace);
    check_ops_on_layout(
        &mut layout,
        [Op::MoveWindowToWorkspace {
            window_id: Some(1),
            workspace_idx: 1,
            focus: true,
        }],
    );

    assert_eq!(marks_for(&layout, 1), vec![String::from("travels")]);
}
#[test]
fn unmark_takes_a_named_mark_off_its_holder_and_bare_unmark_clears_every_window() {
    let options = Options::from_config(&Config::default());
    let mut layout = Layout::with_options(Clock::with_time(Duration::ZERO), options);

    let output = make_test_output("output-test");
    layout.add_output(output.clone(), None);

    let params1 = TestWindowParams::new(1);
    let id1 = params1.id;
    layout.add_window(
        TestWindow::new(params1),
        AddWindowTarget::Auto,
        None,
        None,
        false,
        ActivateWindow::Yes,
    );

    let params2 = TestWindowParams::new(2);
    let id2 = params2.id;
    layout.add_window(
        TestWindow::new(params2),
        AddWindowTarget::Auto,
        None,
        None,
        false,
        ActivateWindow::Yes,
    );

    let workspace = layout.active_workspace_mut().expect("active workspace");
    assert!(workspace.focus_window_by_id(&id1));
    layout.mark_focused(String::from("alpha"), MarkMode::Replace);
    layout.mark_focused(String::from("beta"), MarkMode::Add);

    let workspace = layout.active_workspace_mut().expect("active workspace");
    assert!(workspace.focus_window_by_id(&id2));
    layout.mark_focused(String::from("gamma"), MarkMode::Replace);

    layout.unmark(Some("alpha"));
    assert_eq!(marks_for(&layout, id1), vec![String::from("beta")]);
    assert_eq!(marks_for(&layout, id2), vec![String::from("gamma")]);

    // Bare `unmark` is i3's sweeping form: every mark in the layout goes, not just the ones
    // on whatever happens to be focused. Recorded in
    // `tiri-parity/fixtures/unmark-one-and-unmark-all.parity`.
    let workspace = layout.active_workspace_mut().expect("active workspace");
    assert!(workspace.focus_window_by_id(&id1));
    layout.unmark(None);

    assert!(marks_for(&layout, id1).is_empty());
    assert!(marks_for(&layout, id2).is_empty());
}
#[test]
fn urgent_propagates_to_workspace() {
    let mut layout = check_ops([
        Op::AddOutput(1),
        Op::AddWindow {
            params: TestWindowParams::new(1),
        },
        Op::AddWindow {
            params: TestWindowParams::new(2),
        },
        Op::SetLayoutTabbed,
    ]);

    set_window_urgent(&mut layout, 1, true);

    let workspace = layout.active_workspace().expect("active workspace");
    assert!(
        workspace.is_urgent(),
        "workspace should reflect urgent child state"
    );

    set_window_urgent(&mut layout, 1, false);

    let workspace = layout.active_workspace().expect("active workspace");
    assert!(
        !workspace.is_urgent(),
        "workspace urgency should clear when urgent flag is removed",
    );
}
