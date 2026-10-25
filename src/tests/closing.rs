//! What closing leaves behind: the reflow of the survivors, and the transaction the last
//! window's close animation waits on.

use client::ClientId;
use tiri_ipc::LayoutTreeNode;
use wayland_client::protocol::wl_surface::WlSurface;

use super::*;
use crate::layout::{FocusCommand, LayoutCommand, LayoutMode, WorkspaceTarget};

fn set_up() -> (Fixture, ClientId) {
    let mut f = Fixture::new();
    // A renderer, so the close animations that a real session runs are actually created.
    f.niri_state().backend.headless().add_renderer().unwrap();
    f.add_output(1, (1920, 1080));
    let id = f.add_client();
    (f, id)
}

/// Ack whatever the compositor last asked of every window, so the layout transaction the
/// previous command opened can complete.
fn settle(f: &mut Fixture, id: ClientId, surfaces: &[WlSurface]) {
    for surface in surfaces {
        let configure_size = {
            let window = f.client(id).window(surface);
            window.recent_configures().last().map(|c| c.size)
        };

        let window = f.client(id).window(surface);
        if let Some((w, h)) = configure_size {
            if let (Ok(w), Ok(h)) = (u16::try_from(w), u16::try_from(h)) {
                if w > 0 && h > 0 {
                    window.set_size(w, h);
                }
            }
            window.ack_last();
        }
        window.commit();
    }

    f.double_roundtrip(id);
    f.niri_complete_animations();
}

fn add_window(f: &mut Fixture, id: ClientId, surfaces: &mut Vec<WlSurface>) -> WlSurface {
    let window = f.client(id).create_window();
    let surface = window.surface.clone();
    window.commit();
    f.roundtrip(id);

    let window = f.client(id).window(&surface);
    window.attach_new_buffer();
    window.set_size(200, 200);
    window.ack_last_and_commit();
    f.double_roundtrip(id);

    surfaces.push(surface.clone());
    settle(f, id, surfaces);
    surface
}

fn unmap(f: &mut Fixture, id: ClientId, surface: &WlSurface) {
    let window = f.client(id).window(surface);
    window.attach_null();
    window.commit();
    f.double_roundtrip(id);
}

fn layout_root(f: &mut Fixture) -> LayoutTreeNode {
    f.niri()
        .layout
        .layout_tree()
        .root
        .expect("layout tree root should exist")
}

fn child_widths(node: &LayoutTreeNode) -> Vec<f64> {
    node.children
        .iter()
        .map(|child| child.rect.expect("node should have a rect").width)
        .collect()
}

/// Five windows, the last three inside one split, and the split closed as a whole.
///
/// The three clients answer the close one after the other, so the second and third removals
/// land while the arrange from the first is still waiting on a configure. That arrange is
/// deferred, and running it later is the whole of the reflow: without it the two survivors
/// keep the third of the workspace each had while the split was there, and the space it
/// occupied stays empty.
///
/// The occupied workspace in front matters. Committing a ready layout is an arena-wide pass
/// while the deferred arrange belongs to one workspace, and a frame walks every workspace in
/// turn: the workspace visited first ran the commit, and the one owing itself the arrange was
/// told there had been nothing to commit and kept its old boxes until the next command
/// arranged it by hand.
#[test]
fn closing_a_whole_container_reflows_the_survivors() {
    let (mut f, id) = set_up();

    // A window on the workspace ahead of the one under test.
    let mut elsewhere = Vec::new();
    add_window(&mut f, id, &mut elsewhere);
    f.niri().layout.switch_workspace(WorkspaceTarget::Down);
    f.double_roundtrip(id);

    let mut surfaces = Vec::new();
    add_window(&mut f, id, &mut surfaces);
    add_window(&mut f, id, &mut surfaces);
    let s3 = add_window(&mut f, id, &mut surfaces);

    f.niri()
        .layout
        .layout_command(LayoutCommand::Split(LayoutMode::SplitV));
    f.double_roundtrip(id);
    let s4 = add_window(&mut f, id, &mut surfaces);
    let s5 = add_window(&mut f, id, &mut surfaces);

    let root = layout_root(&mut f);
    assert_eq!(
        root.children.len(),
        3,
        "the root should hold two leaves and the split",
    );

    // `focus parent` selects the split, and closing it closes all three of its windows.
    f.niri().layout.focus_command(FocusCommand::Parent);
    f.double_roundtrip(id);
    let closing = f.niri().layout.close_window_ids_for_active_selection();
    assert_eq!(
        closing.len(),
        3,
        "closing the split should close its three windows"
    );

    for surface in [&s3, &s4, &s5] {
        unmap(&mut f, id, surface);
    }

    // Everything the compositor asked of the survivors, answered — twice, so a relayout that
    // needs one more round trip still gets it.
    let survivors: Vec<WlSurface> = surfaces[..2].to_vec();
    settle(&mut f, id, &survivors);
    settle(&mut f, id, &survivors);

    let widths = child_widths(&layout_root(&mut f));
    assert_eq!(widths.len(), 2);
    assert!(
        (widths[0] - widths[1]).abs() < 2.0,
        "the survivors should split the workspace evenly, got {widths:?}",
    );
    let total: f64 = widths.iter().sum();
    assert!(
        total > 1800.0,
        "the survivors should have grown into the freed space, got {widths:?}",
    );
}
