//! Tests for the normalizer itself.
//!
//! The normalizer decides what counts as a difference, so it is the one component that
//! cannot be validated by the harness that uses it. These tests feed it hand-written trees
//! from both sides — the sway ones are trimmed from real `get_tree` output captured against
//! sway 1.11 — and assert that behaviourally equal states normalize equal, and that
//! behaviourally different ones do not.

use std::collections::HashMap;

use tiri_ipc::{LayoutTree, LayoutTreeLayout, LayoutTreeNode, LayoutTreeRect};

use crate::model::{Layout, Node};
use crate::{erase_decoration, sway, tiri};

const AREA: LayoutTreeRect = LayoutTreeRect {
    x: 0.0,
    y: 0.0,
    width: 1920.0,
    height: 1080.0,
};

fn sway_order(ids: &[(i64, u32)]) -> sway::OpenOrder {
    ids.iter().copied().collect::<HashMap<_, _>>()
}

fn tiri_order(ids: &[(u64, u32)]) -> tiri::OpenOrder {
    ids.iter().copied().collect::<HashMap<_, _>>()
}

fn leaf(window_id: u64, focused: bool, rect: LayoutTreeRect) -> LayoutTreeNode {
    LayoutTreeNode {
        path: Vec::new(),
        layout: None,
        window_id: Some(window_id),
        title: None,
        app_id: None,
        pid: None,
        focused,
        is_floating: false,
        visible: true,
        is_urgent: false,
        is_sticky: false,
        is_scratchpad: false,
        marks: Vec::new(),
        rect: Some(rect),
        percent: None,
        children: Vec::new(),
    }
}

fn container(
    layout: LayoutTreeLayout,
    rect: LayoutTreeRect,
    children: Vec<LayoutTreeNode>,
) -> LayoutTreeNode {
    LayoutTreeNode {
        path: Vec::new(),
        layout: Some(layout),
        window_id: None,
        title: None,
        app_id: None,
        pid: None,
        focused: false,
        is_floating: false,
        visible: true,
        is_urgent: false,
        is_sticky: false,
        is_scratchpad: false,
        marks: Vec::new(),
        rect: Some(rect),
        percent: None,
        children,
    }
}

fn rect(x: f64, y: f64, w: f64, h: f64) -> LayoutTreeRect {
    LayoutTreeRect {
        x,
        y,
        width: w,
        height: h,
    }
}

/// Trimmed from real sway output: `split v` on an empty workspace, then two windows.
/// The workspace carries the orientation and the windows are its direct children.
const SWAY_SPLITV_TWO_WINDOWS: &str = r#"
{
  "id": 1, "type": "root", "layout": "splith", "focused": false, "rect": {"x":0,"y":0,"width":1920,"height":1080},
  "nodes": [
    {
      "id": 2, "type": "output", "name": "HEADLESS-1", "layout": "output", "focused": false,
      "rect": {"x":0,"y":0,"width":1920,"height":1080},
      "nodes": [
        {
          "id": 3, "type": "workspace", "name": "__i3_scratch", "layout": "splith", "focused": false,
          "rect": {"x":0,"y":0,"width":1920,"height":1080}, "nodes": []
        },
        {
          "id": 4, "type": "workspace", "name": "1", "layout": "splitv", "focused": false,
          "rect": {"x":0,"y":0,"width":1920,"height":1080},
          "nodes": [
            {"id": 5, "type": "con", "layout": "none", "focused": false, "visible": true,
             "rect": {"x":0,"y":0,"width":1920,"height":540}, "nodes": []},
            {"id": 6, "type": "con", "layout": "none", "focused": true, "visible": true,
             "rect": {"x":0,"y":540,"width":1920,"height":540}, "nodes": []}
          ]
        }
      ]
    }
  ]
}
"#;

#[test]
fn sway_workspace_orientation_becomes_the_workspace_layout() {
    let ws = sway::normalize(SWAY_SPLITV_TWO_WINDOWS, &sway_order(&[(5, 1), (6, 2)])).unwrap();

    // The root, the output and the scratch workspace are gone; the windows are direct
    // children of a splitv workspace.
    assert_eq!(
        ws.render(),
        "workspace splitv focus=2\n\
         \x20 window 1 0.000,0.000 1.000x0.500\n\
         \x20 window 2 0.000,0.500 1.000x0.500\n"
    );
}

#[test]
fn a_bare_leaf_root_in_tiri_equals_a_workspace_with_one_child_in_sway() {
    // This is the equivalence the whole comparison rests on: tiri keeps the workspace
    // orientation outside the tree when the root is a lone window, sway keeps it on the
    // workspace node. Same state, different representation.
    let tree = LayoutTree {
        workspace_id: None,
        workspace_name: None,
        output: None,
        root: Some(leaf(10, true, rect(0.0, 0.0, 1920.0, 1080.0))),
        floating: Vec::new(),
    };
    let from_tiri =
        tiri::normalize(&tree, Layout::SplitV, false, AREA, &tiri_order(&[(10, 1)])).unwrap();

    let sway_json = r#"
    {"id":1,"type":"root","layout":"splith","focused":false,"rect":{"x":0,"y":0,"width":1920,"height":1080},
     "nodes":[{"id":4,"type":"workspace","name":"1","layout":"splitv","focused":false,
               "rect":{"x":0,"y":0,"width":1920,"height":1080},
               "nodes":[{"id":5,"type":"con","layout":"none","focused":true,"visible":true,
                         "rect":{"x":0,"y":0,"width":1920,"height":1080},"nodes":[]}]}]}
    "#;
    let from_sway = sway::normalize(sway_json, &sway_order(&[(5, 1)])).unwrap();

    assert_eq!(from_tiri.diff(&from_sway), Vec::new());
}

#[test]
fn tiri_container_root_supplies_the_workspace_layout() {
    // When tiri's root is a real container, its layout is the workspace's and its children
    // are the workspace's children — no extra level.
    let tree = LayoutTree {
        workspace_id: None,
        workspace_name: None,
        output: None,
        root: Some(container(
            LayoutTreeLayout::SplitV,
            rect(0.0, 0.0, 1920.0, 1080.0),
            vec![
                leaf(10, false, rect(0.0, 0.0, 1920.0, 540.0)),
                leaf(11, true, rect(0.0, 540.0, 1920.0, 540.0)),
            ],
        )),
        floating: Vec::new(),
    };
    let from_tiri = tiri::normalize(
        &tree,
        Layout::SplitH,
        false,
        AREA,
        &tiri_order(&[(10, 1), (11, 2)]),
    )
    .unwrap();
    let from_sway =
        sway::normalize(SWAY_SPLITV_TWO_WINDOWS, &sway_order(&[(5, 1), (6, 2)])).unwrap();

    assert_eq!(from_tiri.diff(&from_sway), Vec::new());
}

#[test]
fn an_explicit_single_child_split_is_kept_because_sway_keeps_it_too() {
    // Measured: `focus left; split v` on a two-window workspace leaves a splitv container
    // holding one window, and sway does not collapse it. Erasing it here would hide a real
    // difference in how the next window gets placed.
    let tree = LayoutTree {
        workspace_id: None,
        workspace_name: None,
        output: None,
        root: Some(container(
            LayoutTreeLayout::SplitH,
            rect(0.0, 0.0, 1920.0, 1080.0),
            vec![
                container(
                    LayoutTreeLayout::SplitV,
                    rect(0.0, 0.0, 960.0, 1080.0),
                    vec![leaf(10, true, rect(0.0, 0.0, 960.0, 1080.0))],
                ),
                leaf(11, false, rect(960.0, 0.0, 960.0, 1080.0)),
            ],
        )),
        floating: Vec::new(),
    };
    let ws = tiri::normalize(
        &tree,
        Layout::SplitH,
        false,
        AREA,
        &tiri_order(&[(10, 1), (11, 2)]),
    )
    .unwrap();

    assert_eq!(
        ws.render(),
        "workspace splith focus=1\n\
         \x20 splitv 0.000,0.000 0.500x1.000\n\
         \x20   window 1 0.000,0.000 0.500x1.000\n\
         \x20 window 2 0.500,0.000 0.500x1.000\n"
    );
}

#[test]
fn tab_bar_height_is_not_a_difference() {
    // sway insets tabbed children by its own tab bar height (27px at the default font);
    // tiri insets by whatever its tab bar config says. That band is decoration, so after
    // erase_decoration the two must agree.
    let sway_json = r#"
    {"id":1,"type":"root","layout":"splith","focused":false,"rect":{"x":0,"y":0,"width":1920,"height":1080},
     "nodes":[{"id":4,"type":"workspace","name":"1","layout":"tabbed","focused":false,
               "rect":{"x":0,"y":0,"width":1920,"height":1080},
               "nodes":[{"id":5,"type":"con","layout":"none","focused":false,"visible":false,
                         "rect":{"x":0,"y":27,"width":1920,"height":1053},"nodes":[]},
                        {"id":6,"type":"con","layout":"none","focused":true,"visible":true,
                         "rect":{"x":0,"y":27,"width":1920,"height":1053},"nodes":[]}]}]}
    "#;

    let tree = LayoutTree {
        workspace_id: None,
        workspace_name: None,
        output: None,
        root: Some(container(
            LayoutTreeLayout::Tabbed,
            rect(0.0, 0.0, 1920.0, 1080.0),
            vec![
                // tiri's tab bar is a different height, so the content rect differs.
                LayoutTreeNode {
                    visible: false,
                    ..leaf(10, false, rect(0.0, 40.0, 1920.0, 1040.0))
                },
                leaf(11, true, rect(0.0, 40.0, 1920.0, 1040.0)),
            ],
        )),
        floating: Vec::new(),
    };

    let mut from_sway = sway::normalize(sway_json, &sway_order(&[(5, 1), (6, 2)])).unwrap();
    let mut from_tiri = tiri::normalize(
        &tree,
        Layout::Tabbed,
        false,
        AREA,
        &tiri_order(&[(10, 1), (11, 2)]),
    )
    .unwrap();

    // Without erasing decoration they differ, purely because of the band height.
    assert!(!from_tiri.diff(&from_sway).is_empty());

    erase_decoration(&mut from_sway);
    erase_decoration(&mut from_tiri);
    assert_eq!(from_tiri.diff(&from_sway), Vec::new());
}

#[test]
fn which_tab_is_on_top_is_still_a_difference() {
    // Erasing decoration must not erase the one thing a tabbed layout is about.
    let base = |visible_first: bool| LayoutTree {
        workspace_id: None,
        workspace_name: None,
        output: None,
        root: Some(container(
            LayoutTreeLayout::Tabbed,
            rect(0.0, 0.0, 1920.0, 1080.0),
            vec![
                LayoutTreeNode {
                    visible: visible_first,
                    ..leaf(10, visible_first, rect(0.0, 40.0, 1920.0, 1040.0))
                },
                LayoutTreeNode {
                    visible: !visible_first,
                    ..leaf(11, !visible_first, rect(0.0, 40.0, 1920.0, 1040.0))
                },
            ],
        )),
        floating: Vec::new(),
    };
    let order = tiri_order(&[(10, 1), (11, 2)]);

    let mut a = tiri::normalize(&base(true), Layout::Tabbed, false, AREA, &order).unwrap();
    let mut b = tiri::normalize(&base(false), Layout::Tabbed, false, AREA, &order).unwrap();
    erase_decoration(&mut a);
    erase_decoration(&mut b);

    let diff = a.diff(&b);
    assert!(
        diff.iter().any(|d| d.at == "workspace/focus"),
        "focus moved between tabs and must be reported: {diff:?}"
    );
    assert!(
        diff.iter()
            .any(|d| d.expected == "visible" && d.actual == "hidden"),
        "the visible tab changed and must be reported: {diff:?}"
    );
}

#[test]
fn geometry_differences_survive_the_tolerance() {
    // A pixel of rounding is not a difference; a different split ratio is.
    let build = |split: f64| LayoutTree {
        workspace_id: None,
        workspace_name: None,
        output: None,
        root: Some(container(
            LayoutTreeLayout::SplitH,
            rect(0.0, 0.0, 1920.0, 1080.0),
            vec![
                leaf(10, true, rect(0.0, 0.0, split, 1080.0)),
                leaf(11, false, rect(split, 0.0, 1920.0 - split, 1080.0)),
            ],
        )),
        floating: Vec::new(),
    };
    let order = tiri_order(&[(10, 1), (11, 2)]);
    let at = |s| tiri::normalize(&build(s), Layout::SplitH, false, AREA, &order).unwrap();

    assert_eq!(
        at(960.0).diff(&at(961.0)),
        Vec::new(),
        "one pixel is rounding"
    );
    assert!(
        !at(960.0).diff(&at(1200.0)).is_empty(),
        "a different ratio is a real difference"
    );
}

#[test]
fn an_unopened_window_is_an_error_rather_than_a_silent_pass() {
    // If the harness loses track of a window, the comparison must fail loudly: a model
    // missing a window would otherwise look like agreement.
    let err = sway::normalize(SWAY_SPLITV_TWO_WINDOWS, &sway_order(&[(5, 1)]));
    assert!(matches!(err, Err(sway::Error::UnknownWindow(6))));
}

#[test]
fn a_missing_window_is_reported_at_its_position() {
    let two = LayoutTree {
        workspace_id: None,
        workspace_name: None,
        output: None,
        root: Some(container(
            LayoutTreeLayout::SplitH,
            rect(0.0, 0.0, 1920.0, 1080.0),
            vec![
                leaf(10, true, rect(0.0, 0.0, 960.0, 1080.0)),
                leaf(11, false, rect(960.0, 0.0, 960.0, 1080.0)),
            ],
        )),
        floating: Vec::new(),
    };
    let one = LayoutTree {
        root: Some(leaf(10, true, rect(0.0, 0.0, 1920.0, 1080.0))),
        ..two.clone()
    };
    let order = tiri_order(&[(10, 1), (11, 2)]);

    let a = tiri::normalize(&two, Layout::SplitH, false, AREA, &order).unwrap();
    let b = tiri::normalize(&one, Layout::SplitH, false, AREA, &order).unwrap();

    let diff = a.diff(&b);
    assert_eq!(diff[0].at, "workspace");
    assert_eq!(diff[0].expected, "2 children");
    assert_eq!(diff[0].actual, "1 children");
}

/// The fixture format is only trustworthy if reading it back gives what was written.
#[test]
fn rendering_and_parsing_are_inverses() {
    let cases = [
        "workspace splith focus=none\n",
        "\
workspace splitv focus=2
  window 1 0.000,0.000 1.000x0.500
  window 2 0.000,0.500 1.000x0.500
",
        "\
workspace splith focus=3
  tabbed 0.000,0.000 0.500x1.000
    window 1 0.000,0.000 0.500x1.000 hidden
    splitv 0.000,0.000 0.500x1.000
      window 2 0.000,0.000 0.500x0.500 mark:a mark:b
      window 3 0.000,0.500 0.500x0.500
  window 4 0.500,0.000 0.500x1.000 floating
",
    ];

    for case in cases {
        let parsed = crate::model::parse(case).unwrap_or_else(|err| panic!("{err}: {case}"));
        assert_eq!(parsed.render(), case);
        assert_eq!(parsed.diff(&parsed), Vec::new());
    }
}

#[test]
fn a_malformed_fixture_names_the_line_that_broke() {
    let err =
        crate::model::parse("workspace splith focus=1\n      window 1 0,0 1x1\n").unwrap_err();
    assert_eq!(err.line, 2);
    assert_eq!(err.reason, "indentation skips a level");
}

/// Rounding to three decimals must stay well inside the comparison tolerance, or fixtures
/// would report differences that only exist in the file format.
#[test]
fn the_fixture_format_does_not_lose_enough_precision_to_matter() {
    let mut ws =
        crate::model::parse("workspace splith focus=1\n  window 1 0.000,0.000 1.000x1.000\n")
            .unwrap();
    let Node::Window(window) = &mut ws.nodes[0] else {
        unreachable!()
    };
    window.rect.w = 0.333_49;

    let round_tripped = crate::model::parse(&ws.render()).unwrap();
    assert_eq!(ws.diff(&round_tripped), Vec::new());
}

#[test]
fn a_fixture_round_trips_and_yields_its_script() {
    let text = "\
# recorded from sway 1.11

$ open
workspace splith focus=1
  window 1 0.000,0.000 1.000x1.000

$ split v
workspace splitv focus=1
  window 1 0.000,0.000 1.000x1.000
";

    let fixture = crate::Fixture::parse(text).unwrap();
    assert_eq!(fixture.source, "sway 1.11");
    assert_eq!(fixture.script(), "open\nsplit v\n");
    assert_eq!(fixture.render(), text);
}

#[test]
fn a_fixture_without_a_source_is_rejected() {
    let err = crate::Fixture::parse("$ open\nworkspace splith focus=none\n").unwrap_err();
    assert!(err.reason.contains("recorded from"), "{err}");
}

#[test]
fn a_broken_model_points_at_the_command_it_followed() {
    let text = "\
# recorded from sway 1.11

$ split v
workspace nonsense focus=none
";
    let err = crate::Fixture::parse(text).unwrap_err();
    assert!(err.reason.contains("split v"), "{err}");
}

/// The point of recording focus as a position: two states that differ only in which
/// container is selected must not look the same.
#[test]
fn focus_on_a_container_is_distinguishable_from_focus_on_another() {
    let json = |focused_path: &str| {
        let (ws, outer, inner) = match focused_path {
            "workspace" => ("true", "false", "false"),
            "outer" => ("false", "true", "false"),
            _ => ("false", "false", "true"),
        };
        format!(
            r#"{{"id":1,"type":"root","layout":"splith","focused":false,
                 "rect":{{"x":0,"y":0,"width":1920,"height":1080}},
             "nodes":[{{"id":4,"type":"workspace","name":"1","layout":"splith","focused":{ws},
                       "rect":{{"x":0,"y":0,"width":1920,"height":1080}},
                       "nodes":[{{"id":5,"type":"con","layout":"splitv","focused":{outer},
                                 "rect":{{"x":0,"y":0,"width":1920,"height":1080}},
                        "nodes":[{{"id":6,"type":"con","layout":"splith","focused":{inner},
                                  "rect":{{"x":0,"y":0,"width":1920,"height":1080}},
                          "nodes":[{{"id":7,"type":"con","layout":"none","focused":false,
                                    "visible":true,
                                    "rect":{{"x":0,"y":0,"width":1920,"height":1080}},
                                    "nodes":[]}}]}}]}}]}}]}}"#
        )
    };
    let order = sway_order(&[(7, 1)]);
    let at = |which| sway::normalize(&json(which), &order).unwrap();

    let workspace = at("workspace");
    let outer = at("outer");
    let inner = at("inner");

    assert_eq!(workspace.focused, crate::model::Focus::Container(vec![]));
    assert_eq!(outer.focused, crate::model::Focus::Container(vec![0]));
    assert_eq!(inner.focused, crate::model::Focus::Container(vec![0, 0]));

    // The trees are identical; only focus differs, and that must be reported.
    for (a, b) in [(&workspace, &outer), (&outer, &inner), (&workspace, &inner)] {
        let diff = a.diff(b);
        assert_eq!(diff.len(), 1, "{diff:?}");
        assert_eq!(diff[0].at, "workspace/focus");
    }
}

#[test]
fn a_focused_container_round_trips_through_the_fixture_format() {
    let text = "\
workspace splith focus=@0/1
  splitv 0.000,0.000 1.000x1.000
    window 1 0.000,0.000 1.000x0.500
    splith 0.000,0.500 1.000x0.500
      window 2 0.000,0.500 1.000x0.500
";
    let parsed = crate::model::parse(text).unwrap();
    assert_eq!(parsed.focused, crate::model::Focus::Container(vec![0, 1]));
    assert_eq!(parsed.render(), text);

    let workspace_focused = crate::model::parse("workspace splith focus=@\n").unwrap();
    assert_eq!(
        workspace_focused.focused,
        crate::model::Focus::Container(vec![])
    );
    assert_eq!(workspace_focused.render(), "workspace splith focus=@\n");
}

/// Erasing the tab bar must not erase the layout underneath it.
#[test]
fn a_split_inside_a_tab_keeps_its_proportions() {
    // Both sides put a splith inside a tabbed container. sway's numbers below a tab are not
    // self-consistent — the leaves are inset by one decoration band and the split holding
    // them by two — so the erasure has to divide that out rather than flatten it away.
    let sway_json = r#"
    {"id":1,"type":"root","layout":"splith","focused":false,"rect":{"x":0,"y":0,"width":1000,"height":1000},
     "nodes":[{"id":4,"type":"workspace","name":"1","layout":"splith","focused":false,
               "rect":{"x":0,"y":0,"width":1000,"height":1000},
      "nodes":[{"id":5,"type":"con","layout":"tabbed","focused":false,
                "rect":{"x":0,"y":0,"width":1000,"height":1000},
        "nodes":[{"id":6,"type":"con","layout":"splith","focused":false,
                  "rect":{"x":0,"y":40,"width":1000,"height":960},
          "nodes":[{"id":7,"type":"con","layout":"none","focused":true,"visible":true,
                    "rect":{"x":0,"y":20,"width":500,"height":980},"nodes":[]},
                   {"id":8,"type":"con","layout":"none","focused":false,"visible":true,
                    "rect":{"x":500,"y":20,"width":500,"height":980},"nodes":[]}]}]}]}]}
    "#;

    let tiri = |split: f64| LayoutTree {
        workspace_id: None,
        workspace_name: None,
        output: None,
        root: Some(container(
            LayoutTreeLayout::SplitH,
            rect(0.0, 0.0, 1920.0, 1080.0),
            vec![container(
                LayoutTreeLayout::Tabbed,
                rect(0.0, 0.0, 1920.0, 1080.0),
                vec![container(
                    LayoutTreeLayout::SplitH,
                    // A different tab bar height, and a different one again inside.
                    rect(0.0, 30.0, 1920.0, 1050.0),
                    vec![
                        leaf(10, true, rect(0.0, 30.0, split, 1050.0)),
                        leaf(11, false, rect(split, 30.0, 1920.0 - split, 1050.0)),
                    ],
                )],
            )],
        )),
        floating: Vec::new(),
    };
    let order = tiri_order(&[(10, 1), (11, 2)]);
    let normalized = |split| {
        let mut model = tiri::normalize(&tiri(split), Layout::SplitH, false, AREA, &order).unwrap();
        erase_decoration(&mut model);
        model
    };
    let mut from_sway = sway::normalize(sway_json, &sway_order(&[(7, 1), (8, 2)])).unwrap();
    erase_decoration(&mut from_sway);

    // Same 50/50 split, despite three different decoration bands between them.
    assert_eq!(from_sway.diff(&normalized(960.0)), Vec::new());

    // And a different split is still reported — this is what flattening used to hide.
    let differences = from_sway.diff(&normalized(1730.0));
    assert!(
        !differences.is_empty(),
        "a 90/10 split inside a tab must not compare equal to 50/50"
    );
}
