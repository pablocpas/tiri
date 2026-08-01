//! Normalize sway's `get_tree` into the observable model.
//!
//! Shapes here were checked against sway 1.11 running headless; see `docs/design/parity.md`
//! for the scenarios and what each rule is derived from.

use std::collections::HashMap;

use serde::Deserialize;

use crate::model::{Container, FracRect, Layout, Node, Window, WindowId, Workspace};

/// The subset of a `get_tree` node this model needs.
#[derive(Debug, Deserialize)]
pub struct SwayNode {
    pub id: i64,
    #[serde(rename = "type")]
    pub kind: String,
    pub name: Option<String>,
    pub layout: String,
    pub focused: bool,
    /// Present on leaves only; sway omits it on workspaces and containers.
    #[serde(default)]
    pub visible: Option<bool>,
    #[serde(default)]
    pub marks: Vec<String>,
    pub rect: SwayRect,
    #[serde(default)]
    pub nodes: Vec<SwayNode>,
    #[serde(default)]
    pub floating_nodes: Vec<SwayNode>,
}

#[derive(Debug, Clone, Copy, Deserialize)]
pub struct SwayRect {
    pub x: f64,
    pub y: f64,
    pub width: f64,
    pub height: f64,
}

/// Maps sway node ids to the order the harness opened them.
pub type OpenOrder = HashMap<i64, WindowId>;

#[derive(Debug)]
pub enum Error {
    Json(serde_json::Error),
    NoWorkspace,
    /// A window appeared that the harness never opened, so it has no stable identity.
    UnknownWindow(i64),
}

impl From<serde_json::Error> for Error {
    fn from(e: serde_json::Error) -> Self {
        Error::Json(e)
    }
}

/// Normalize the focused workspace out of a `get_tree` reply.
pub fn normalize(json: &str, order: &OpenOrder) -> Result<Workspace, Error> {
    let root: SwayNode = serde_json::from_str(json)?;
    let ws = find_focused_workspace(&root).ok_or(Error::NoWorkspace)?;

    // A workspace is a container with an orientation, so its layout is the workspace's.
    // `output` and `none` never appear here.
    let layout = Layout::from_sway(&ws.layout).unwrap_or(Layout::SplitH);
    let area = ws.rect;

    let mut focused = None;
    let mut nodes = Vec::new();
    for child in &ws.nodes {
        nodes.push(convert(child, area, order, false, &mut focused)?);
    }
    for child in &ws.floating_nodes {
        nodes.push(convert(child, area, order, true, &mut focused)?);
    }

    Ok(Workspace {
        layout,
        focused,
        nodes,
    })
}

/// The focused workspace, skipping `root`, `output` and sway's `__i3_scratch` holding pen.
fn find_focused_workspace(node: &SwayNode) -> Option<&SwayNode> {
    if node.kind == "workspace" {
        let hidden = node.name.as_deref().is_some_and(|n| n.starts_with("__"));
        if !hidden && contains_focus(node) {
            return Some(node);
        }
        return None;
    }
    node.nodes
        .iter()
        .chain(node.floating_nodes.iter())
        .find_map(find_focused_workspace)
}

fn contains_focus(node: &SwayNode) -> bool {
    node.focused
        || node
            .nodes
            .iter()
            .chain(node.floating_nodes.iter())
            .any(contains_focus)
}

fn convert(
    node: &SwayNode,
    area: SwayRect,
    order: &OpenOrder,
    floating: bool,
    focused: &mut Option<WindowId>,
) -> Result<Node, Error> {
    // A leaf is a node with no children of its own. sway reports `layout: none` for them.
    let is_leaf = node.nodes.is_empty() && node.floating_nodes.is_empty();

    if is_leaf {
        let id = *order.get(&node.id).ok_or(Error::UnknownWindow(node.id))?;
        if node.focused {
            *focused = Some(id);
        }
        return Ok(Node::Window(Window {
            id,
            rect: frac(node.rect, area),
            // sway omits `visible` on anything that is not a leaf; a leaf without it is on
            // screen.
            visible: node.visible.unwrap_or(true),
            floating,
            marks: node.marks.clone(),
        }));
    }

    let layout = Layout::from_sway(&node.layout).unwrap_or(Layout::SplitH);
    let mut nodes = Vec::new();
    for child in node.nodes.iter().chain(node.floating_nodes.iter()) {
        nodes.push(convert(child, area, order, floating, focused)?);
    }

    Ok(Node::Container(Container {
        layout,
        rect: frac(node.rect, area),
        nodes,
    }))
}

fn frac(r: SwayRect, area: SwayRect) -> FracRect {
    FracRect {
        x: (r.x - area.x) / area.width,
        y: (r.y - area.y) / area.height,
        w: r.width / area.width,
        h: r.height / area.height,
    }
}
