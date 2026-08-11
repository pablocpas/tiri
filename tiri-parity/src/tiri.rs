//! Normalize tiri's IPC layout tree into the observable model.

use std::collections::HashMap;

use tiri_ipc::{
    LayoutTree, LayoutTreeFloatingRootKind, LayoutTreeLayout, LayoutTreeNode, LayoutTreeRect,
};

use crate::model::{Container, Focus, FracRect, Layout, Node, Window, WindowId, Workspace};

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
    workspace_selected: bool,
    area: LayoutTreeRect,
    order: &OpenOrder,
) -> Result<Workspace, Error> {
    if area.width <= 0.0 || area.height <= 0.0 {
        return Err(Error::NoArea);
    }

    let mut nodes = Vec::new();

    // When the root is itself a container, its children are the workspace's children and
    // its layout is the workspace layout. When it is a bare leaf, it is the workspace's
    // only child.
    // The root container is the workspace, so focus on it is focus on the workspace and its
    // children's positions are the workspace's.
    let mut focused = Focus::Nothing;
    let layout = match &tree.root {
        Some(root) if root.window_id.is_none() => {
            let layout = root.layout.map(layout_of).unwrap_or(workspace_layout);
            if root.focused {
                focused = Focus::Container(Vec::new());
            }
            for (idx, child) in root.children.iter().enumerate() {
                nodes.push(convert(
                    child,
                    area,
                    order,
                    false,
                    &mut vec![idx],
                    &mut focused,
                )?);
            }
            layout
        }
        Some(root) => {
            nodes.push(convert(
                root,
                area,
                order,
                false,
                &mut vec![0],
                &mut focused,
            )?);
            workspace_layout
        }
        None => workspace_layout,
    };

    // A workspace whose only child is a window has no node for tiri to mark, so the tree
    // cannot say the workspace is what `focus parent` selected. The caller knows.
    if workspace_selected {
        focused = Focus::Container(Vec::new());
    }

    for root in &tree.floating {
        // Tiri gives every floating group a container root; sway reports a lone floating
        // window as the window itself. IPC says whether that root is scaffolding or a real,
        // addressable container, so normalization never guesses from child count.
        let root = match (root.floating_root_kind, root.children.as_slice()) {
            (Some(LayoutTreeFloatingRootKind::ImplicitWindowGroup), [only]) => only,
            // Backward compatibility with producers from before root provenance was
            // published. Current Tiri always sends `Some` for floating roots.
            (None, [only]) if root.window_id.is_none() => only,
            _ => root,
        };
        let mut path = vec![nodes.len()];
        nodes.push(convert(root, area, order, true, &mut path, &mut focused)?);
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
    path: &mut Vec<usize>,
    focused: &mut Focus,
) -> Result<Node, Error> {
    if let Some(window_id) = node.window_id {
        let id = *order
            .get(&window_id)
            .ok_or(Error::UnknownWindow(window_id))?;
        if node.focused {
            *focused = Focus::Window(id);
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
    if node.focused {
        *focused = Focus::Container(path.clone());
    }
    let mut nodes = Vec::new();
    for (idx, child) in node.children.iter().enumerate() {
        path.push(idx);
        let converted = convert(child, area, order, floating, path, focused);
        path.pop();
        nodes.push(converted?);
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
