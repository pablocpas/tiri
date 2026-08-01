//! Normalize tiri's IPC layout tree into the observable model.

use std::collections::HashMap;

use tiri_ipc::{LayoutTree, LayoutTreeLayout, LayoutTreeNode, LayoutTreeRect};

use crate::model::{Container, FracRect, Layout, Node, Window, WindowId, Workspace};

/// Maps tiri window ids to the order the harness opened them.
pub type OpenOrder = HashMap<u64, WindowId>;

#[derive(Debug)]
pub enum Error {
    /// A window appeared that the harness never opened, so it has no stable identity.
    UnknownWindow(u64),
    /// The tree carried no geometry, so nothing can be compared.
    NoArea,
}

fn layout_of(l: LayoutTreeLayout) -> Layout {
    match l {
        LayoutTreeLayout::SplitH => Layout::SplitH,
        LayoutTreeLayout::SplitV => Layout::SplitV,
        LayoutTreeLayout::Tabbed => Layout::Tabbed,
        LayoutTreeLayout::Stacked => Layout::Stacked,
    }
}

/// Normalize a layout tree, given the working area it was laid out in.
///
/// `workspace_layout` is the workspace's own orientation, which tiri keeps outside the tree
/// when the root is a bare leaf. That case is the whole reason this argument exists: sway
/// always has a workspace node carrying the layout, and tiri represents the same state as a
/// leaf root plus this value.
pub fn normalize(
    tree: &LayoutTree,
    workspace_layout: Layout,
    area: LayoutTreeRect,
    order: &OpenOrder,
) -> Result<Workspace, Error> {
    if area.width <= 0.0 || area.height <= 0.0 {
        return Err(Error::NoArea);
    }

    let mut focused = None;
    let mut nodes = Vec::new();

    // When the root is itself a container, its children are the workspace's children and
    // its layout is the workspace layout. When it is a bare leaf, it is the workspace's
    // only child.
    let layout = match &tree.root {
        Some(root) if root.window_id.is_none() => {
            let layout = root.layout.map(layout_of).unwrap_or(workspace_layout);
            for child in &root.children {
                nodes.push(convert(child, area, order, false, &mut focused)?);
            }
            layout
        }
        Some(root) => {
            nodes.push(convert(root, area, order, false, &mut focused)?);
            workspace_layout
        }
        None => workspace_layout,
    };

    for root in &tree.floating {
        nodes.push(convert(root, area, order, true, &mut focused)?);
    }

    Ok(Workspace {
        layout,
        focused,
        nodes,
    })
}

fn convert(
    node: &LayoutTreeNode,
    area: LayoutTreeRect,
    order: &OpenOrder,
    floating: bool,
    focused: &mut Option<WindowId>,
) -> Result<Node, Error> {
    if let Some(window_id) = node.window_id {
        let id = *order
            .get(&window_id)
            .ok_or(Error::UnknownWindow(window_id))?;
        if node.focused {
            *focused = Some(id);
        }
        return Ok(Node::Window(Window {
            id,
            rect: frac(node.rect, area),
            visible: node.visible,
            floating: floating || node.is_floating,
            marks: node.marks.clone(),
        }));
    }

    let layout = node.layout.map(layout_of).unwrap_or(Layout::SplitH);
    let mut nodes = Vec::new();
    for child in &node.children {
        nodes.push(convert(child, area, order, floating, focused)?);
    }

    Ok(Node::Container(Container {
        layout,
        rect: frac(node.rect, area),
        nodes,
    }))
}

fn frac(rect: Option<LayoutTreeRect>, area: LayoutTreeRect) -> FracRect {
    let Some(r) = rect else {
        return FracRect {
            x: 0.0,
            y: 0.0,
            w: 0.0,
            h: 0.0,
        };
    };
    FracRect {
        x: (r.x - area.x) / area.width,
        y: (r.y - area.y) / area.height,
        w: r.width / area.width,
        h: r.height / area.height,
    }
}
