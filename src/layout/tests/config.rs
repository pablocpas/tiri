use super::*;

#[test]
fn large_negative_height_change() {
    let ops = [
        Op::AddOutput(1),
        Op::AddWindow {
            params: TestWindowParams::new(1),
        },
        Op::SetWindowHeight {
            id: None,
            change: SizeChange::AdjustProportion(-1e129),
        },
    ];

    let mut options = Options::default();
    options.layout.border.off = false;
    options.layout.border.width = 1.;

    check_ops_with_options(options, ops);
}
#[test]
fn large_max_size() {
    let ops = [
        Op::AddOutput(1),
        Op::AddWindow {
            params: TestWindowParams {
                min_max_size: (Size::from((0, 0)), Size::from((i32::MAX, i32::MAX))),
                ..TestWindowParams::new(1)
            },
        },
    ];

    let mut options = Options::default();
    options.layout.border.off = false;
    options.layout.border.width = 1.;

    check_ops_with_options(options, ops);
}
#[test]
fn config_change_updates_cached_sizes() {
    let mut config = Config::default();
    let border = &mut config.layout.border;
    border.off = false;
    border.width = 2.;

    let mut layout = Layout::new(Clock::default(), &config);

    Op::AddWindow {
        params: TestWindowParams {
            bbox: Rectangle::from_size(Size::from((1280, 200))),
            ..TestWindowParams::new(1)
        },
    }
    .apply(&mut layout);

    config.layout.border.width = 4.;
    layout.update_config(&config);

    layout.verify_invariants();
}
#[test]
fn preset_height_change_removes_preset() {
    let mut config = Config::default();
    config.layout.preset_window_heights = vec![PresetSize::Fixed(1), PresetSize::Fixed(2)];

    let mut layout = Layout::new(Clock::default(), &config);

    let ops = [
        Op::AddOutput(1),
        Op::AddWindow {
            params: TestWindowParams::new(1),
        },
        Op::AddWindow {
            params: TestWindowParams::new(2),
        },
        Op::ConsumeOrExpelWindowLeft { id: None },
        Op::SwitchPresetWindowHeight { id: None },
        Op::SwitchPresetWindowHeight { id: None },
    ];
    for op in ops {
        op.apply(&mut layout);
    }

    // Leave only one.
    config.layout.preset_window_heights = vec![PresetSize::Fixed(1)];

    layout.update_config(&config);

    layout.verify_invariants();
}
#[test]
fn fixed_height_takes_max_non_auto_into_account() {
    let ops = [
        Op::AddOutput(1),
        Op::AddWindow {
            params: TestWindowParams::new(0),
        },
        Op::SetWindowHeight {
            id: Some(0),
            change: SizeChange::SetFixed(704),
        },
        Op::AddWindow {
            params: TestWindowParams::new(1),
        },
        Op::ConsumeOrExpelWindowLeft { id: None },
    ];

    let options = Options {
        layout: tiri_config::Layout {
            border: tiri_config::Border {
                off: false,
                width: 4.,
                ..Default::default()
            },
            gaps: 0.,
            ..Default::default()
        },
        ..Default::default()
    };
    check_ops_with_options(options, ops);
}
#[test]
fn set_width_fixed_negative() {
    let ops = [
        Op::AddOutput(3),
        Op::AddWindow {
            params: TestWindowParams::new(3),
        },
        Op::ToggleWindowFloating { id: Some(3) },
        Op::SetColumnWidth(SizeChange::SetFixed(-100)),
    ];
    check_ops(ops);
}
#[test]
fn set_height_fixed_negative() {
    let ops = [
        Op::AddOutput(3),
        Op::AddWindow {
            params: TestWindowParams::new(3),
        },
        Op::ToggleWindowFloating { id: Some(3) },
        Op::SetWindowHeight {
            id: None,
            change: SizeChange::SetFixed(-100),
        },
    ];
    check_ops(ops);
}
#[test]
fn preset_column_width_fixed_correct_with_border() {
    let ops = [
        Op::AddOutput(0),
        Op::AddWindow {
            params: TestWindowParams::new(0),
        },
        Op::SwitchPresetColumnWidth,
    ];

    let options = Options {
        layout: tiri_config::Layout {
            preset_column_widths: vec![PresetSize::Fixed(500)],
            ..Default::default()
        },
        ..Default::default()
    };
    let mut layout = check_ops_with_options(options, ops);

    let win = layout.windows().next().unwrap().1;
    let base_width = win.requested_size().unwrap().w;

    // Add border.
    let options = Options {
        layout: tiri_config::Layout {
            preset_column_widths: vec![PresetSize::Fixed(500)],
            border: tiri_config::Border {
                off: false,
                width: 5.,
                ..Default::default()
            },
            ..Default::default()
        },
        ..Default::default()
    };
    layout.update_options(options);

    // With border, the window gets less size.
    let win = layout.windows().next().unwrap().1;
    let bordered_width = win.requested_size().unwrap().w;
    assert!(bordered_width <= base_width);

    // Preset widths are ignored in i3-style tiling, so toggling doesn't change size.
    layout.toggle_width(true);
    let win = layout.windows().next().unwrap().1;
    assert_eq!(win.requested_size().unwrap().w, bordered_width);
}
#[test]
fn preset_column_width_reset_after_set_width() {
    let ops = [
        Op::AddOutput(0),
        Op::AddWindow {
            params: TestWindowParams::new(0),
        },
        Op::SwitchPresetColumnWidth,
        Op::SetWindowWidth {
            id: None,
            change: SizeChange::AdjustFixed(-10),
        },
        Op::SwitchPresetColumnWidth,
    ];

    let options = Options {
        layout: tiri_config::Layout {
            preset_column_widths: vec![PresetSize::Fixed(500), PresetSize::Fixed(1000)],
            ..Default::default()
        },
        ..Default::default()
    };
    let layout = check_ops_with_options(options, ops);
    let win = layout.windows().next().unwrap().1;
    let width_after_resize = win.requested_size().unwrap().w;
    assert!(width_after_resize > 0);
}
#[test]
fn tabs_with_different_border() {
    let ops = [
        Op::AddOutput(1),
        Op::AddWindow {
            params: TestWindowParams {
                rules: Some(Box::new(ResolvedWindowRules {
                    border: tiri_config::BorderRule {
                        on: true,
                        ..Default::default()
                    },
                    ..ResolvedWindowRules::default()
                })),
                ..TestWindowParams::new(2)
            },
        },
        Op::SwitchPresetWindowHeight { id: None },
        Op::ToggleColumnTabbedDisplay,
        Op::AddWindow {
            params: TestWindowParams::new(3),
        },
        Op::ConsumeOrExpelWindowLeft { id: None },
    ];

    let options = Options {
        layout: tiri_config::Layout {
            struts: Struts {
                left: FloatOrInt(0.),
                right: FloatOrInt(0.),
                top: FloatOrInt(20000.),
                bottom: FloatOrInt(0.),
            },
            ..Default::default()
        },
        ..Default::default()
    };
    check_ops_with_options(options, ops);
}

#[test]
fn struts_reserve_space_at_the_edges_of_the_working_area() {
    let ops = [
        Op::AddOutput(1),
        Op::AddWindow {
            params: TestWindowParams::new(1),
        },
    ];

    let options = Options {
        layout: tiri_config::Layout {
            struts: Struts {
                left: FloatOrInt(10.),
                right: FloatOrInt(20.),
                top: FloatOrInt(30.),
                bottom: FloatOrInt(40.),
            },
            ..Default::default()
        },
        ..Default::default()
    };

    let layout = check_ops_with_options(options, ops);
    let area = layout.active_workspace().unwrap().working_area();

    // The 1280x720 test output, minus each edge's strut.
    assert_eq!(area.loc.x, 10.);
    assert_eq!(area.loc.y, 30.);
    assert_eq!(area.size.w, 1280. - 10. - 20.);
    assert_eq!(area.size.h, 720. - 30. - 40.);
}

#[test]
fn struts_larger_than_the_output_leave_no_working_area() {
    let ops = [
        Op::AddOutput(1),
        Op::AddWindow {
            params: TestWindowParams::new(1),
        },
    ];

    let options = Options {
        layout: tiri_config::Layout {
            struts: Struts {
                left: FloatOrInt(0.),
                right: FloatOrInt(0.),
                top: FloatOrInt(20000.),
                bottom: FloatOrInt(0.),
            },
            ..Default::default()
        },
        ..Default::default()
    };

    let layout = check_ops_with_options(options, ops);
    let area = layout.active_workspace().unwrap().working_area();

    // Clamped rather than negative: a window still has to be given some rectangle.
    assert_eq!(area.size.h, 0.);
    assert_eq!(area.size.w, 1280.);
}

#[test]
fn a_config_reload_re_derives_the_working_area_from_the_new_struts() {
    let mut config = Config::default();

    let mut layout = Layout::new(Clock::default(), &config);
    Op::AddOutput(1).apply(&mut layout);
    Op::AddWindow {
        params: TestWindowParams::new(1),
    }
    .apply(&mut layout);

    assert_eq!(
        layout.active_workspace().unwrap().working_area().size.h,
        720.
    );

    // Struts are the one layout option that changes the box everything is laid out in, so a
    // reload has to re-derive it instead of keeping the box it was handed before.
    config.layout.struts.top = FloatOrInt(100.);
    layout.update_config(&config);

    assert_eq!(
        layout.active_workspace().unwrap().working_area().size.h,
        620.
    );
    assert_eq!(layout.active_workspace().unwrap().working_area().loc.y, 100.);

    layout.verify_invariants();
}

#[test]
fn struts_survive_a_workspace_being_parked_without_outputs() {
    let mut config = Config::default();
    config.layout.struts.top = FloatOrInt(50.);

    let mut layout = Layout::new(Clock::default(), &config);
    Op::AddOutput(1).apply(&mut layout);
    Op::AddWindow {
        params: TestWindowParams::new(1),
    }
    .apply(&mut layout);

    let attached = layout.active_workspace().unwrap().working_area();

    // Park the workspace by taking its output away, then give it one back. The area it comes
    // back with has to be the one it would have had all along, not one derived from the
    // placeholder size it was holding while parked.
    Op::RemoveOutput(1).apply(&mut layout);
    Op::AddOutput(1).apply(&mut layout);

    assert_eq!(layout.active_workspace().unwrap().working_area(), attached);
    layout.verify_invariants();
}


#[test]
fn the_two_answers_to_every_tile_in_a_workspace_agree() {
    let ops = [
        Op::AddOutput(1),
        Op::AddWindow {
            params: TestWindowParams::new(1),
        },
        Op::AddWindow {
            params: TestWindowParams::new(2),
        },
        Op::ConsumeOrExpelWindowLeft { id: None },
        Op::AddWindow {
            params: TestWindowParams {
                is_floating: true,
                ..TestWindowParams::new(3)
            },
        },
        Op::AddWindow {
            params: TestWindowParams {
                is_floating: true,
                ..TestWindowParams::new(4)
            },
        },
    ];

    let mut layout = check_ops(ops);
    let ws = layout.active_workspace_mut().unwrap();

    // Same tiles and same order, both sides of the workspace included. They are one walk now,
    // and the point of the test is to notice if they stop being one.
    let read: Vec<_> = ws
        .tiles()
        .map(|tile| *tile.window().id())
        .collect();
    let written: Vec<_> = ws
        .tiles_mut()
        .map(|tile| *tile.window().id())
        .collect();

    assert_eq!(read, written);
    assert_eq!(read.len(), 4, "two tiled and two floating");
}
