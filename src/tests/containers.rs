use std::collections::BTreeSet;

use crate::layout::ContainerLayout;
use client::ClientId;
use tiri_ipc::{LayoutTreeLayout, LayoutTreeNode};
use wayland_client::protocol::wl_surface::WlSurface;

use super::*;

fn set_up() -> (Fixture, ClientId) {
    let mut f = Fixture::new();
    f.add_output(1, (1920, 1080));
    let id = f.add_client();
    (f, id)
}

fn add_window(f: &mut Fixture, id: ClientId, size: (u16, u16)) -> WlSurface {
    let window = f.client(id).create_window();
    let surface = window.surface.clone();
    window.commit();
    f.roundtrip(id);

    let window = f.client(id).window(&surface);
    window.attach_new_buffer();
    window.set_size(size.0, size.1);
    window.ack_last_and_commit();
    f.double_roundtrip(id);

    surface
}

fn active_workspace_window_count(f: &mut Fixture) -> usize {
    f.niri()
        .layout
        .active_workspace()
        .expect("active workspace")
        .windows()
        .count()
}

fn active_window_id(f: &mut Fixture) -> u64 {
    f.niri()
        .layout
        .active_workspace()
        .expect("active workspace")
        .active_window()
        .expect("active window")
        .id()
        .get()
}

fn layout_root(f: &mut Fixture) -> LayoutTreeNode {
    f.niri()
        .layout
        .layout_tree()
        .root
        .expect("layout tree root should exist")
}

fn leaf_count(node: &LayoutTreeNode) -> usize {
    if node.window_id.is_some() {
        1
    } else {
        node.children.iter().map(leaf_count).sum()
    }
}

fn collect_leaf_ids(node: &LayoutTreeNode, ids: &mut Vec<u64>) {
    if let Some(id) = node.window_id {
        ids.push(id);
    }

    for child in &node.children {
        collect_leaf_ids(child, ids);
    }
}

fn focused_leaf_count(node: &LayoutTreeNode) -> usize {
    let this = usize::from(node.window_id.is_some() && node.focused);
    this + node.children.iter().map(focused_leaf_count).sum::<usize>()
}

fn focused_node_count(node: &LayoutTreeNode) -> usize {
    let this = usize::from(node.focused);
    this + node.children.iter().map(focused_node_count).sum::<usize>()
}

fn focused_node_path(node: &LayoutTreeNode) -> Option<Vec<usize>> {
    fn visit(node: &LayoutTreeNode, path: &mut Vec<usize>) -> Option<Vec<usize>> {
        if node.focused {
            return Some(path.clone());
        }

        for (idx, child) in node.children.iter().enumerate() {
            path.push(idx);
            let found = visit(child, path);
            path.pop();
            if found.is_some() {
                return found;
            }
        }

        None
    }

    visit(node, &mut Vec::new())
}

fn first_leaf(node: &LayoutTreeNode) -> Option<&LayoutTreeNode> {
    if node.window_id.is_some() {
        return Some(node);
    }

    node.children.iter().find_map(first_leaf)
}

#[test]
fn layout_tree_ipc_exposes_output_geometry_paths_and_percents() {
    let (mut f, id) = set_up();
    add_window(&mut f, id, (100, 100));
    add_window(&mut f, id, (200, 200));

    let tree = f.niri().layout.layout_tree();
    assert!(tree.output.is_some());
    assert!(tree.floating.is_empty());

    let root = tree.root.expect("layout tree root should exist");
    assert_eq!(root.path, Vec::<usize>::new());
    assert!(root.rect.is_some());
    assert_eq!(root.children.len(), 2);

    for (idx, child) in root.children.iter().enumerate() {
        assert_eq!(child.path, vec![idx]);
        assert!(child.rect.is_some());
        assert!(
            child
                .percent
                .is_some_and(|percent| percent > 0.0 && percent <= 1.0),
            "child should expose a sane parent percent: {child:?}",
        );
    }
}

#[test]
fn layout_tree_ipc_exposes_floating_nodes() {
    let (mut f, id) = set_up();
    add_window(&mut f, id, (100, 100));

    f.niri().layout.toggle_window_floating(None);
    f.double_roundtrip(id);

    let tree = f.niri().layout.layout_tree();
    assert_eq!(tree.floating.len(), 1);

    let floating_root = &tree.floating[0];
    assert_eq!(floating_root.path, vec![0]);
    assert!(floating_root.is_floating);
    assert!(floating_root.rect.is_some());

    let leaf = first_leaf(floating_root).expect("floating tree should contain a leaf");
    assert!(leaf.is_floating);
    assert!(leaf.window_id.is_some());
    assert!(leaf.rect.is_some());
}

#[test]
fn split_vertical_creates_nested_splitv_subtree() {
    let (mut f, id) = set_up();
    add_window(&mut f, id, (110, 110));
    add_window(&mut f, id, (220, 220));

    f.niri().layout.split_vertical();
    f.double_roundtrip(id);
    add_window(&mut f, id, (330, 330));

    let root = layout_root(&mut f);
    assert_eq!(root.layout, Some(LayoutTreeLayout::SplitH));
    assert_eq!(leaf_count(&root), 3);
    assert_eq!(root.children.len(), 2);

    let nested_splitv = root
        .children
        .iter()
        .find(|child| child.layout == Some(LayoutTreeLayout::SplitV))
        .expect("expected a nested SplitV container");
    assert_eq!(nested_splitv.children.len(), 2);
    assert!(nested_splitv
        .children
        .iter()
        .all(|child| child.window_id.is_some()));
}

#[test]
fn split_horizontal_creates_three_root_leaf_children() {
    let (mut f, id) = set_up();
    add_window(&mut f, id, (110, 110));
    add_window(&mut f, id, (220, 220));

    f.niri().layout.split_horizontal();
    f.double_roundtrip(id);
    add_window(&mut f, id, (330, 330));

    let root = layout_root(&mut f);
    assert_eq!(root.layout, Some(LayoutTreeLayout::SplitH));
    assert_eq!(leaf_count(&root), 3);
    assert_eq!(root.children.len(), 2);
    assert!(root.children[0].window_id.is_some());
    assert_eq!(root.children[1].layout, Some(LayoutTreeLayout::SplitH));
    assert_eq!(root.children[1].children.len(), 2);
    assert!(root.children[1]
        .children
        .iter()
        .all(|child| child.window_id.is_some()));
}

#[test]
fn change_layout_to_tabbed_keeps_all_windows_and_moves_focus() {
    let (mut f, id) = set_up();
    add_window(&mut f, id, (100, 100));
    add_window(&mut f, id, (200, 200));
    add_window(&mut f, id, (300, 300));

    f.niri().layout.set_layout_mode(ContainerLayout::Tabbed);
    f.double_roundtrip(id);

    // Measured against sway 1.11: a layout command issued from a window builds a container
    // holding the workspace's children; the workspace keeps its own orientation.
    let root = layout_root(&mut f);
    assert_eq!(root.layout, Some(LayoutTreeLayout::SplitH));
    assert_eq!(root.children.len(), 1);
    assert_eq!(root.children[0].layout, Some(LayoutTreeLayout::Tabbed));
    assert_eq!(root.children[0].children.len(), 3);
    assert_eq!(leaf_count(&root), 3);
    assert_eq!(focused_leaf_count(&root), 1);
}

#[test]
fn change_layout_to_stacked_keeps_all_windows_and_moves_focus() {
    let (mut f, id) = set_up();
    add_window(&mut f, id, (100, 100));
    add_window(&mut f, id, (200, 200));
    add_window(&mut f, id, (300, 300));

    f.niri().layout.set_layout_mode(ContainerLayout::Stacked);
    f.double_roundtrip(id);

    // Measured against sway 1.11: see the tabbed test above — same rule.
    let root = layout_root(&mut f);
    assert_eq!(root.layout, Some(LayoutTreeLayout::SplitH));
    assert_eq!(root.children.len(), 1);
    assert_eq!(root.children[0].layout, Some(LayoutTreeLayout::Stacked));
    assert_eq!(root.children[0].children.len(), 3);
    assert_eq!(leaf_count(&root), 3);
    assert_eq!(focused_leaf_count(&root), 1);
}

#[test]
fn toggle_split_layout_twice_restores_root_layout() {
    let (mut f, id) = set_up();
    add_window(&mut f, id, (120, 120));
    add_window(&mut f, id, (240, 240));

    let initial_root = layout_root(&mut f);
    assert_eq!(initial_root.layout, Some(LayoutTreeLayout::SplitH));
    assert_eq!(initial_root.children.len(), 2);

    // Measured against sway 1.11: the toggle builds a container under the workspace and
    // then flips that container, so the workspace itself stays splith throughout.
    f.niri().layout.toggle_split_layout();
    f.double_roundtrip(id);
    let toggled_root = layout_root(&mut f);
    assert_eq!(toggled_root.layout, Some(LayoutTreeLayout::SplitH));
    assert_eq!(toggled_root.children.len(), 1);
    assert_eq!(
        toggled_root.children[0].layout,
        Some(LayoutTreeLayout::SplitV)
    );
    assert_eq!(leaf_count(&toggled_root), 2);

    f.niri().layout.toggle_split_layout();
    f.double_roundtrip(id);
    let restored_root = layout_root(&mut f);
    assert_eq!(restored_root.layout, Some(LayoutTreeLayout::SplitH));
    assert_eq!(restored_root.children.len(), 1);
    assert_eq!(
        restored_root.children[0].layout,
        Some(LayoutTreeLayout::SplitH)
    );
    assert_eq!(leaf_count(&restored_root), 2);
    assert_eq!(leaf_count(&restored_root), 2);
}

#[test]
fn focus_parent_then_child_in_split_preserves_focused_window() {
    let (mut f, id) = set_up();
    add_window(&mut f, id, (100, 100));
    add_window(&mut f, id, (200, 200));

    f.niri().layout.split_vertical();
    f.double_roundtrip(id);
    add_window(&mut f, id, (300, 300));

    let before = active_window_id(&mut f);
    f.niri().layout.focus_parent();
    f.double_roundtrip(id);
    f.niri().layout.focus_child();
    f.double_roundtrip(id);
    let after = active_window_id(&mut f);

    assert_eq!(before, after);
}

#[test]
fn focus_parent_marks_nested_container_focused_in_layout_tree() {
    let (mut f, id) = set_up();
    add_window(&mut f, id, (100, 100));
    add_window(&mut f, id, (200, 200));

    f.niri().layout.split_vertical();
    f.double_roundtrip(id);
    add_window(&mut f, id, (300, 300));

    let focused_window_before = active_window_id(&mut f);

    f.niri().layout.focus_parent();
    f.double_roundtrip(id);

    let root = layout_root(&mut f);
    assert_eq!(root.layout, Some(LayoutTreeLayout::SplitH));
    assert_eq!(focused_node_count(&root), 1);
    assert_eq!(focused_leaf_count(&root), 0);
    assert_eq!(focused_node_path(&root), Some(vec![1]));
    assert_eq!(root.children[1].layout, Some(LayoutTreeLayout::SplitV));
    assert!(root.children[1].focused);
    assert_eq!(
        active_window_id(&mut f),
        focused_window_before,
        "focus_parent should not move real surface focus away from the active leaf",
    );
}

#[test]
fn focus_parent_twice_bubbles_from_nested_split_to_parent_split_in_layout_tree() {
    let (mut f, id) = set_up();
    add_window(&mut f, id, (100, 100));
    add_window(&mut f, id, (200, 200));

    f.niri().layout.split_vertical();
    f.double_roundtrip(id);
    add_window(&mut f, id, (300, 300));
    add_window(&mut f, id, (400, 400));

    f.niri().layout.focus_left();
    f.double_roundtrip(id);
    add_window(&mut f, id, (500, 500));

    f.niri().layout.focus_right();
    f.double_roundtrip(id);
    let focused_window_before = active_window_id(&mut f);

    f.niri().layout.focus_parent();
    f.double_roundtrip(id);

    let after_first_parent = layout_root(&mut f);
    assert_eq!(after_first_parent.layout, Some(LayoutTreeLayout::SplitH));
    assert_eq!(focused_node_count(&after_first_parent), 1);
    assert_eq!(focused_leaf_count(&after_first_parent), 0);
    assert!(
        after_first_parent
            .children
            .iter()
            .any(|child| { child.layout == Some(LayoutTreeLayout::SplitV) && child.focused }),
        "first focus_parent should expose the nested SplitV as the focused node in the layout tree",
    );

    f.niri().layout.focus_parent();
    f.double_roundtrip(id);

    let after_second_parent = layout_root(&mut f);
    assert_eq!(after_second_parent.layout, Some(LayoutTreeLayout::SplitH));
    assert_eq!(focused_node_count(&after_second_parent), 1);
    assert_eq!(focused_leaf_count(&after_second_parent), 0);
    assert_eq!(focused_node_path(&after_second_parent), Some(Vec::new()));
    assert!(after_second_parent.focused);
    assert_eq!(
        active_window_id(&mut f),
        focused_window_before,
        "bubbling container focus must keep the active surface on the same leaf",
    );
}

#[test]
fn mixed_container_ops_keep_tree_leaf_ids_unique() {
    let (mut f, id) = set_up();
    add_window(&mut f, id, (120, 120));
    add_window(&mut f, id, (200, 200));
    add_window(&mut f, id, (280, 280));

    f.niri().layout.split_vertical();
    f.double_roundtrip(id);
    f.niri().layout.set_layout_mode(ContainerLayout::Tabbed);
    f.double_roundtrip(id);
    f.niri().layout.focus_window_down_or_top();
    f.double_roundtrip(id);
    f.niri().layout.set_layout_mode(ContainerLayout::SplitV);
    f.double_roundtrip(id);
    f.niri().layout.split_horizontal();
    f.double_roundtrip(id);
    add_window(&mut f, id, (360, 360));

    let root = layout_root(&mut f);
    let mut leaf_ids = Vec::new();
    collect_leaf_ids(&root, &mut leaf_ids);

    let unique = leaf_ids.iter().copied().collect::<BTreeSet<_>>();
    assert_eq!(
        leaf_ids.len(),
        unique.len(),
        "leaf window ids must be unique"
    );
    assert_eq!(leaf_ids.len(), leaf_count(&root));
    assert_eq!(leaf_ids.len(), active_workspace_window_count(&mut f));
}
