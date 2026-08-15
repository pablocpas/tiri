use std::collections::{HashMap, HashSet};
use std::fmt::{self, Write as _};

use insta::assert_snapshot;
use tiri_config::Config;

use super::*;
use crate::layout::LayoutElement as _;
use crate::utils::spawning::store_and_increase_nofile_rlimit;
use crate::utils::with_toplevel_role;

#[test]
fn simple_no_workspaces() {
    let mut f = Fixture::new();

    let id = f.add_client();
    let window = f.client(id).create_window();
    let surface = window.surface.clone();
    window.commit();
    f.roundtrip(id);

    let window = f.client(id).window(&surface);
    assert_snapshot!(
        window.format_recent_configures(),
        @"size: 0 × 0, bounds: 0 × 0, states: []"
    );

    window.attach_new_buffer();
    window.set_size(100, 100);
    window.ack_last_and_commit();
    f.double_roundtrip(id);

    let window = f.client(id).window(&surface);
    assert_snapshot!(
        window.format_recent_configures(),
        @"size: 1264 × 704, bounds: 1264 × 704, states: []"
    );
}

#[test]
fn simple() {
    let mut f = Fixture::new();
    f.add_output(1, (1920, 1080));

    let id = f.add_client();
    let window = f.client(id).create_window();
    let surface = window.surface.clone();
    window.commit();
    f.roundtrip(id);

    let window = f.client(id).window(&surface);
    assert_snapshot!(
        window.format_recent_configures(),
        @"size: 0 × 0, bounds: 800 × 600, states: []"
    );

    window.attach_new_buffer();
    window.ack_last_and_commit();
    f.double_roundtrip(id);

    let window = f.client(id).window(&surface);
    assert_snapshot!(
        window.format_recent_configures(),
        @"size: 1904 × 1064, bounds: 1904 × 1064, states: [Activated]"
    );
}

#[test]
#[should_panic(expected = "Protocol error 3 on object xdg_surface")]
fn dont_ack_initial_configure() {
    let mut f = Fixture::new();
    f.add_output(1, (1920, 1080));

    let id = f.add_client();
    let window = f.client(id).create_window();
    let surface = window.surface.clone();
    window.commit();
    f.roundtrip(id);

    let window = f.client(id).window(&surface);
    window.attach_new_buffer();
    // Don't ack the configure.
    window.commit();
    f.double_roundtrip(id);
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
enum WantFullscreen {
    No,
    UnsetBeforeInitial,
    BeforeInitial(Option<&'static str>),
    UnsetAfterInitial,
    AfterInitial(Option<&'static str>),
}

impl fmt::Display for WantFullscreen {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            WantFullscreen::No => write!(f, "U")?,
            WantFullscreen::UnsetBeforeInitial => write!(f, "BU")?,
            WantFullscreen::UnsetAfterInitial => write!(f, "AU")?,
            WantFullscreen::BeforeInitial(m) => write!(f, "B{}", m.unwrap_or("N"))?,
            WantFullscreen::AfterInitial(m) => write!(f, "A{}", m.unwrap_or("N"))?,
        }
        Ok(())
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
enum SetParent {
    BeforeInitial(&'static str),
    AfterInitial(&'static str),
}

impl fmt::Display for SetParent {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            SetParent::BeforeInitial(m) => write!(f, "B{m}")?,
            SetParent::AfterInitial(m) => write!(f, "A{m}")?,
        }
        Ok(())
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
enum DefaultSize {
    WindowChooses,
    Proportion(&'static str),
    Fixed(&'static str),
}

impl fmt::Display for DefaultSize {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            DefaultSize::WindowChooses => write!(f, "U"),
            DefaultSize::Proportion(prop) => write!(f, "P{prop}"),
            DefaultSize::Fixed(fixed) => write!(f, "F{fixed}"),
        }
    }
}

const TARGET_OUTPUT_AND_WORKSPACE_CASE_COUNT: usize = 3 * 3 * 3 * 9 * 5;
const TARGET_SIZE_CASE_COUNT: usize = 3 * 5 * 2 * 4 * 4 * 2 * 2;

#[derive(Clone, Copy, Debug)]
struct TargetOutputAndWorkspaceCase {
    open_on_workspace: Option<&'static str>,
    open_on_output: Option<&'static str>,
    open_fullscreen: Option<&'static str>,
    want_fullscreen: WantFullscreen,
    set_parent: Option<SetParent>,
}

#[derive(Clone, Copy, Debug)]
struct TargetSizeCase {
    open_fullscreen: Option<&'static str>,
    want_fullscreen: WantFullscreen,
    open_floating: Option<&'static str>,
    default_width: Option<DefaultSize>,
    default_height: Option<DefaultSize>,
    border: bool,
    tabbed: bool,
}

#[derive(Clone, Debug)]
struct TargetOutputAndWorkspaceOutcome {
    snapshot_suffix: String,
    snapshot_desc: String,
    snapshot: String,
    final_monitor: String,
    final_workspace: String,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
struct TargetOutputPlacementKey {
    open_on_workspace: Option<&'static str>,
    open_on_output: Option<&'static str>,
    want_fullscreen: WantFullscreen,
    set_parent: Option<SetParent>,
}

fn target_output_and_workspace_cases() -> Vec<TargetOutputAndWorkspaceCase> {
    let open_on_workspace = [None, Some("1"), Some("2")];
    let open_on_output = [None, Some("1"), Some("2")];
    let open_fullscreen = [None, Some("false"), Some("true")];
    let want_fullscreen = [
        WantFullscreen::No,
        WantFullscreen::UnsetBeforeInitial, // GTK 4
        WantFullscreen::BeforeInitial(None),
        WantFullscreen::BeforeInitial(Some("1")),
        WantFullscreen::BeforeInitial(Some("2")),
        WantFullscreen::UnsetAfterInitial,
        // mpv, osu!
        WantFullscreen::AfterInitial(None),
        WantFullscreen::AfterInitial(Some("1")),
        WantFullscreen::AfterInitial(Some("2")),
    ];
    let set_parent = [
        None,
        Some(SetParent::BeforeInitial("1")),
        Some(SetParent::BeforeInitial("2")),
        Some(SetParent::AfterInitial("1")),
        Some(SetParent::AfterInitial("2")),
    ];

    let mut cases = Vec::with_capacity(TARGET_OUTPUT_AND_WORKSPACE_CASE_COUNT);
    for ws in open_on_workspace {
        for out in open_on_output {
            for fs in open_fullscreen {
                for wfs in want_fullscreen {
                    for sp in set_parent {
                        cases.push(TargetOutputAndWorkspaceCase {
                            open_on_workspace: ws,
                            open_on_output: out,
                            open_fullscreen: fs,
                            want_fullscreen: wfs,
                            set_parent: sp,
                        });
                    }
                }
            }
        }
    }

    cases
}

fn target_size_cases() -> Vec<TargetSizeCase> {
    let open_fullscreen = [None, Some("false"), Some("true")];
    let want_fullscreen = [
        WantFullscreen::No,
        WantFullscreen::UnsetBeforeInitial, // GTK 4
        WantFullscreen::BeforeInitial(None),
        WantFullscreen::UnsetAfterInitial,
        // mpv, osu!
        WantFullscreen::AfterInitial(None),
    ];
    let open_floating = [None, Some("true")];
    let default_column_width = [
        None,
        Some(DefaultSize::WindowChooses),
        Some(DefaultSize::Proportion("0.25")),
        Some(DefaultSize::Fixed("1000")),
    ];
    let default_window_height = [
        None,
        Some(DefaultSize::WindowChooses),
        Some(DefaultSize::Proportion("0.5")),
        Some(DefaultSize::Fixed("500")),
    ];
    let border = [false, true];
    let tabbed = [false, true];

    let mut cases = Vec::with_capacity(TARGET_SIZE_CASE_COUNT);
    for fs in open_fullscreen {
        for wfs in want_fullscreen {
            for of in open_floating {
                for dw in default_column_width {
                    for dh in default_window_height {
                        for b in border {
                            for t in tabbed {
                                cases.push(TargetSizeCase {
                                    open_fullscreen: fs,
                                    want_fullscreen: wfs,
                                    open_floating: of,
                                    default_width: dw,
                                    default_height: dh,
                                    border: b,
                                    tabbed: t,
                                });
                            }
                        }
                    }
                }
            }
        }
    }

    cases
}

fn assert_target_output_and_workspace_snapshot(outcome: TargetOutputAndWorkspaceOutcome) {
    let mut settings = insta::Settings::clone_current();
    settings.set_prepend_module_to_snapshot(false);
    settings.set_snapshot_suffix(outcome.snapshot_suffix);
    settings.set_description(outcome.snapshot_desc);
    let _guard = settings.bind_to_scope();
    assert_snapshot!(
        "niri__tests__window_opening__check_target_output_and_workspace",
        outcome.snapshot
    );
}

#[test]
fn target_output_and_workspaces() {
    if std::env::var_os("RUN_SLOW_TESTS").is_none() {
        eprintln!("ignoring slow test");
        return;
    }

    store_and_increase_nofile_rlimit();

    // Here we test a massive powerset of settings that can affect where a window opens:
    //
    // * open-on-workspace
    // * open-on-output
    // * has parent (windows will open next to their parent)
    // * want fullscreen (windows can request the target fullscreen output)
    // * open-fullscreen (can deny the fullscreen request)
    let cases = target_output_and_workspace_cases();
    assert_eq!(cases.len(), TARGET_OUTPUT_AND_WORKSPACE_CASE_COUNT);

    let mut seen_suffixes = HashSet::with_capacity(cases.len());
    let mut placements_without_open_fullscreen =
        HashMap::<TargetOutputPlacementKey, (String, String)>::new();

    for case in cases {
        let outcome = run_target_output_and_workspace_case(case);
        assert!(
            seen_suffixes.insert(outcome.snapshot_suffix.clone()),
            "duplicate snapshot suffix generated for target-output/workspace matrix: {}",
            outcome.snapshot_suffix
        );

        let key = TargetOutputPlacementKey {
            open_on_workspace: case.open_on_workspace,
            open_on_output: case.open_on_output,
            want_fullscreen: case.want_fullscreen,
            set_parent: case.set_parent,
        };
        let placement = (
            outcome.final_monitor.clone(),
            outcome.final_workspace.clone(),
        );
        if let Some(prev) = placements_without_open_fullscreen.insert(key, placement.clone()) {
            assert_eq!(
                prev, placement,
                "window placement changed when only open-fullscreen changed: {key:?}"
            );
        }

        assert_target_output_and_workspace_snapshot(outcome);
    }
}

fn run_target_output_and_workspace_case(
    case: TargetOutputAndWorkspaceCase,
) -> TargetOutputAndWorkspaceOutcome {
    let mut snapshot_desc = Vec::new();
    let mut snapshot_suffix = Vec::new();

    let mut config = String::from(
        r##"
output "headless-2" {
    layout {
        border {
            on
        }
    }
}

workspace "ws-1" {
    open-on-output "headless-1"
}

workspace "ws-2" {
    open-on-output "headless-2"

    layout {
        border {
            width 10
        }

        default-column-width {
            fixed 500
        }
    }
}

window-rule {
    exclude title="parent"

"##,
    );

    if let Some(x) = case.open_on_workspace {
        writeln!(config, "    open-on-workspace \"ws-{x}\"").unwrap();
        snapshot_suffix.push(format!("ws{x}"));
    }

    if let Some(x) = case.open_on_output {
        writeln!(config, "    open-on-output \"headless-{x}\"").unwrap();
        snapshot_suffix.push(format!("out{x}"));
    }

    if let Some(x) = case.open_fullscreen {
        writeln!(config, "    open-fullscreen {x}").unwrap();

        let x = if x == "true" { "T" } else { "F" };
        snapshot_suffix.push(format!("fs{x}"));
    }
    config.push('}');

    match &case.want_fullscreen {
        WantFullscreen::No => (),
        x => {
            snapshot_desc.push(format!("want fullscreen: {x}"));
            snapshot_suffix.push(format!("wfs{x}"));
        }
    }

    if let Some(set_parent) = case.set_parent {
        let mon = match set_parent {
            SetParent::BeforeInitial(mon) => mon,
            SetParent::AfterInitial(mon) => mon,
        };
        write!(
            config,
            "

window-rule {{
    match title=\"parent\"
    open-on-output \"headless-{mon}\"
}}"
        )
        .unwrap();

        snapshot_desc.push(format!("set parent: {set_parent}"));
        snapshot_suffix.push(format!("sp{set_parent}"));
    }

    snapshot_desc.push(format!("config:{config}"));

    let config = Config::parse_mem(&config).unwrap();

    let mut f = Fixture::with_config(config);
    f.add_output(1, (1280, 720));
    f.add_output(2, (1920, 1080));

    let id = f.add_client();

    // To get output names.
    f.roundtrip(id);

    let mut parent = None;
    if case.set_parent.is_some() {
        let window = f.client(id).create_window();
        let surface = window.surface.clone();
        parent = Some(window.xdg_toplevel.clone());
        window.set_title("parent");
        window.commit();
        f.roundtrip(id);

        let window = f.client(id).window(&surface);
        window.attach_new_buffer();
        window.ack_last_and_commit();
        f.roundtrip(id);
    }

    let client = f.client(id);
    let window = client.create_window();
    let surface = window.surface.clone();

    if let Some(SetParent::BeforeInitial(_)) = case.set_parent {
        client.window(&surface).set_parent(parent.as_ref());
    }

    if let WantFullscreen::UnsetBeforeInitial = case.want_fullscreen {
        client.window(&surface).unset_fullscreen();
    } else if let WantFullscreen::BeforeInitial(mon) = case.want_fullscreen {
        let output = mon.map(|mon| client.output(&format!("headless-{mon}")));
        client.window(&surface).set_fullscreen(output.as_ref());
    }

    client.window(&surface).commit();
    f.roundtrip(id);

    let client = f.client(id);
    let initial = client.window(&surface).format_recent_configures();

    if let Some(SetParent::AfterInitial(_)) = case.set_parent {
        client.window(&surface).set_parent(parent.as_ref());
    }

    if let WantFullscreen::UnsetAfterInitial = case.want_fullscreen {
        client.window(&surface).unset_fullscreen();
    } else if let WantFullscreen::AfterInitial(mon) = case.want_fullscreen {
        let output = mon.map(|mon| client.output(&format!("headless-{mon}")));
        client.window(&surface).set_fullscreen(output.as_ref());
    }

    let window = client.window(&surface);
    window.attach_new_buffer();
    let serial = window.configures_received.last().unwrap().0;
    window.ack_last_and_commit();
    f.double_roundtrip(id);

    // Commit to the post-initial configures.
    let window = f.client(id).window(&surface);
    let new_serial = window.configures_received.last().unwrap().0;
    if new_serial != serial {
        window.ack_last_and_commit();
        f.double_roundtrip(id);
    }

    let niri = f.niri();
    let (mon, ws_idx, ws, mapped) = niri
        .layout
        .workspaces()
        .find_map(|(mon, ws_idx, ws)| {
            ws.windows().find_map(|win| {
                if with_toplevel_role(win.toplevel(), |role| {
                    role.title.as_deref() != Some("parent")
                }) {
                    Some((mon, ws_idx, ws, win))
                } else {
                    None
                }
            })
        })
        .unwrap();
    let is_fullscreen = mapped.sizing_mode().is_fullscreen();
    let win = mapped.window.clone();
    let mon = mon.unwrap().output_name().clone();
    let ws = ws.name().cloned().unwrap_or(String::from("unnamed"));

    let window = f.client(id).window(&surface);
    let post_map = window.format_recent_configures();

    // If the window ended up fullscreen, unfullscreen it and output the configure.
    //
    // A window leaving fullscreen into floating gets the size it mapped with, clamped to
    // sway's floating minimum of 75x50. These clients commit a 1x1 buffer whatever they were
    // configured at, so 75x50 is the floor doing its job on a window that really did map
    // that small — not a size anyone computed. A real client commits the size it was told.
    let mut post_unfullscreen = String::new();
    if is_fullscreen {
        f.niri().layout.set_fullscreen(&win, false);
        f.double_roundtrip(id);

        let window = f.client(id).window(&surface);
        post_unfullscreen = format!(
            "\n\nunfullscreen configure:\n{}",
            window.format_recent_configures()
        );
    }

    let snapshot = format!(
        "\
final monitor: {mon}
final workspace: {ws_idx} ({ws})

initial configure:
{initial}

post-map configures:
{post_map}{post_unfullscreen}",
    );

    TargetOutputAndWorkspaceOutcome {
        snapshot_suffix: snapshot_suffix.join("-"),
        snapshot_desc: snapshot_desc.join("\n"),
        snapshot,
        final_monitor: mon,
        final_workspace: format!("{ws_idx} ({ws})"),
    }
}

#[test]
fn target_size() {
    if std::env::var_os("RUN_SLOW_TESTS").is_none() {
        eprintln!("ignoring slow test");
        return;
    }

    store_and_increase_nofile_rlimit();

    // Here we test a massive powerset of settings that can affect the window size:
    //
    // * want fullscreen
    // * open-fullscreen
    // * open-floating
    // * default-column-width
    // * border
    // * default-column-display normal, tabbed
    let cases = target_size_cases();
    assert_eq!(cases.len(), TARGET_SIZE_CASE_COUNT);

    for case in cases {
        check_target_size(
            case.open_fullscreen,
            case.want_fullscreen,
            case.open_floating,
            case.default_width,
            case.default_height,
            case.border,
            case.tabbed,
        );
    }
}

#[allow(clippy::too_many_arguments)]
fn check_target_size(
    open_fullscreen: Option<&str>,
    want_fullscreen: WantFullscreen,
    open_floating: Option<&str>,
    default_width: Option<DefaultSize>,
    default_height: Option<DefaultSize>,
    border: bool,
    tabbed: bool,
) {
    let mut snapshot_desc = Vec::new();
    let mut snapshot_suffix = Vec::new();

    let mut config = String::from(
        r##"
window-rule {
"##,
    );

    if let Some(x) = open_fullscreen {
        writeln!(config, "    open-fullscreen {x}").unwrap();

        let x = if x == "true" { "T" } else { "F" };
        snapshot_suffix.push(format!("fs{x}"));
    }

    if let Some(x) = open_floating {
        writeln!(config, "    open-floating {x}").unwrap();

        let x = if x == "true" { "T" } else { "F" };
        snapshot_suffix.push(format!("of{x}"));
    }

    if let Some(x) = default_width {
        let value = match x {
            DefaultSize::WindowChooses => String::new(),
            DefaultSize::Proportion(prop) => format!("proportion {prop};"),
            DefaultSize::Fixed(fixed) => format!("fixed {fixed};"),
        };
        writeln!(config, "    default-column-width {{ {value} }}").unwrap();

        snapshot_suffix.push(format!("dw{x}"));
    }

    if let Some(x) = default_height {
        let value = match x {
            DefaultSize::WindowChooses => String::new(),
            DefaultSize::Proportion(prop) => format!("proportion {prop};"),
            DefaultSize::Fixed(fixed) => format!("fixed {fixed};"),
        };
        writeln!(config, "    default-window-height {{ {value} }}").unwrap();

        snapshot_suffix.push(format!("dh{x}"));
    }

    if border {
        writeln!(config, "    border {{ on; }}").unwrap();
        snapshot_suffix.push(String::from("b"));
    }

    if tabbed {
        writeln!(config, "    default-column-display \"tabbed\"").unwrap();
    }

    config.push('}');

    match &want_fullscreen {
        WantFullscreen::No => (),
        x => {
            snapshot_desc.push(format!("want fullscreen: {x}"));
            snapshot_suffix.push(format!("wfs{x}"));
        }
    }

    if tabbed {
        config.push_str(
            "\n
layout {
    tab-indicator {
        place-within-column
    }
}",
        );
        snapshot_suffix.push(String::from("t"));
    }

    snapshot_desc.push(format!("config:{config}"));

    let config = Config::parse_mem(&config).unwrap();

    let mut f = Fixture::with_config(config);
    f.add_output(1, (1280, 720));
    f.add_output(2, (1920, 1080));

    let id = f.add_client();

    // To get output names.
    f.roundtrip(id);

    let client = f.client(id);
    let window = client.create_window();
    let surface = window.surface.clone();

    if let WantFullscreen::UnsetBeforeInitial = want_fullscreen {
        client.window(&surface).unset_fullscreen();
    } else if let WantFullscreen::BeforeInitial(mon) = want_fullscreen {
        let output = mon.map(|mon| client.output(&format!("headless-{mon}")));
        client.window(&surface).set_fullscreen(output.as_ref());
    }

    client.window(&surface).commit();
    f.roundtrip(id);

    let client = f.client(id);
    let initial = client.window(&surface).format_recent_configures();

    if let WantFullscreen::UnsetAfterInitial = want_fullscreen {
        client.window(&surface).unset_fullscreen();
    } else if let WantFullscreen::AfterInitial(mon) = want_fullscreen {
        let output = mon.map(|mon| client.output(&format!("headless-{mon}")));
        client.window(&surface).set_fullscreen(output.as_ref());
    }

    let window = client.window(&surface);
    window.attach_new_buffer();
    let serial = window.configures_received.last().unwrap().0;
    window.ack_last_and_commit();
    f.double_roundtrip(id);

    // Commit to the post-initial configures.
    let window = f.client(id).window(&surface);
    let new_serial = window.configures_received.last().unwrap().0;
    if new_serial != serial {
        window.ack_last_and_commit();
        f.double_roundtrip(id);
    }

    let window = f.client(id).window(&surface);
    let post_map = window.format_recent_configures();

    // If the window ended up fullscreen, unfullscreen it and output the configure.
    let mut post_unfullscreen = String::new();
    let mapped = f.niri().layout.windows().next().unwrap().1;
    let is_fullscreen = mapped.sizing_mode().is_fullscreen();
    let win = mapped.window.clone();
    if is_fullscreen {
        f.niri().layout.set_fullscreen(&win, false);
        f.double_roundtrip(id);

        let window = f.client(id).window(&surface);
        post_unfullscreen = format!(
            "\n\nunfullscreen configure:\n{}",
            window.format_recent_configures()
        );
    }

    let snapshot = format!(
        "\
initial configure:
{initial}

post-map configures:
{post_map}{post_unfullscreen}",
    );

    let mut settings = insta::Settings::clone_current();
    settings.set_prepend_module_to_snapshot(false);
    settings.set_snapshot_suffix(snapshot_suffix.join("-"));
    settings.set_description(snapshot_desc.join("\n"));
    let _guard = settings.bind_to_scope();
    assert_snapshot!("niri__tests__window_opening__check_target_size", snapshot);
}
