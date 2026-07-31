use smithay::utils::{Logical, Point, Rectangle};

use super::{ContainerTree, Layout, NodeData, NodeKey};
use crate::utils::with_toplevel_role;
use crate::window::Mapped;
use tiri_ipc::{LayoutTreeLayout, LayoutTreeNode, LayoutTreeRect};

impl ContainerTree<Mapped> {
    pub(in crate::layout) fn layout_tree(&self) -> Option<LayoutTreeNode> {
        let root_key = self.root_node_key()?;
        let focused_key = self.selected_node_key();
        Some(self.build_layout_tree_node(
            root_key,
            focused_key,
            &mut Vec::new(),
            0,
            None,
            Point::default(),
            false,
        ))
    }

    pub(in crate::layout) fn layout_tree_unfocused(&self) -> Option<LayoutTreeNode> {
        let root_key = self.root_node_key()?;
        Some(self.build_layout_tree_node(
            root_key,
            None,
            &mut Vec::new(),
            0,
            None,
            Point::default(),
            false,
        ))
    }

    pub(in crate::layout) fn layout_tree_with_context(
        &self,
        focused_key: Option<NodeKey>,
        path: &mut Vec<usize>,
        path_prefix_len: usize,
        offset: Point<f64, Logical>,
        is_floating: bool,
    ) -> Option<LayoutTreeNode> {
        let root_key = self.root_node_key()?;
        Some(self.build_layout_tree_node(
            root_key,
            focused_key,
            path,
            path_prefix_len,
            None,
            offset,
            is_floating,
        ))
    }

    #[allow(clippy::too_many_arguments)]
    fn build_layout_tree_node(
        &self,
        node_key: NodeKey,
        focused_key: Option<NodeKey>,
        path: &mut Vec<usize>,
        path_prefix_len: usize,
        percent: Option<f64>,
        offset: Point<f64, Logical>,
        is_floating: bool,
    ) -> LayoutTreeNode {
        match self.get_node(node_key) {
            Some(NodeData::Leaf(tile)) => {
                let window = tile.window();
                let (title, app_id) = with_toplevel_role(window.toplevel(), |role| {
                    (role.title.clone(), role.app_id.clone())
                });

                LayoutTreeNode {
                    path: path.clone(),
                    layout: None,
                    window_id: Some(window.id().get()),
                    title,
                    app_id,
                    pid: window.credentials().map(|credentials| credentials.pid),
                    focused: focused_key == Some(node_key),
                    is_floating,
                    visible: self
                        .leaf_layouts()
                        .iter()
                        .find(|info| info.key == node_key)
                        .is_none_or(|info| info.visible),
                    is_urgent: window.is_urgent(),
                    is_sticky: tile.is_sticky(),
                    is_scratchpad: tile.is_scratchpad(),
                    marks: tile.marks().to_vec(),
                    rect: self.node_rect(node_key, &path[path_prefix_len..], offset),
                    percent,
                    children: Vec::new(),
                }
            }
            Some(NodeData::Container(container)) => {
                let child_count = container.child_count();
                let percents_sum: f64 = container.child_percents_slice().iter().copied().sum();
                let percents =
                    self.get_normalized_child_percents(node_key, child_count, percents_sum);
                let mut children = Vec::with_capacity(child_count);

                for (idx, child_key) in container.children().iter().enumerate() {
                    path.push(idx);
                    children.push(self.build_layout_tree_node(
                        *child_key,
                        focused_key,
                        path,
                        path_prefix_len,
                        percents.get(idx).copied(),
                        offset,
                        is_floating,
                    ));
                    path.pop();
                }

                LayoutTreeNode {
                    path: path.clone(),
                    layout: Some(layout_to_ipc(container.layout())),
                    window_id: None,
                    title: None,
                    app_id: None,
                    pid: None,
                    focused: focused_key == Some(node_key),
                    is_floating,
                    visible: children.iter().any(|child| child.visible),
                    is_urgent: children.iter().any(|child| child.is_urgent),
                    is_sticky: children.iter().any(|child| child.is_sticky),
                    is_scratchpad: children.iter().any(|child| child.is_scratchpad),
                    marks: Vec::new(),
                    rect: self.node_rect(node_key, &path[path_prefix_len..], offset),
                    percent,
                    children,
                }
            }
            None => LayoutTreeNode {
                path: path.clone(),
                layout: None,
                window_id: None,
                title: None,
                app_id: None,
                pid: None,
                focused: false,
                is_floating,
                visible: false,
                is_urgent: false,
                is_sticky: false,
                is_scratchpad: false,
                marks: Vec::new(),
                rect: None,
                percent,
                children: Vec::new(),
            },
        }
    }

    fn node_rect(
        &self,
        node_key: NodeKey,
        path: &[usize],
        offset: Point<f64, Logical>,
    ) -> Option<LayoutTreeRect> {
        let rect = match self.get_node(node_key)? {
            NodeData::Leaf(_) => self
                .leaf_layouts()
                .iter()
                .find(|info| info.key == node_key)
                .map(|info| info.rect)
                .or_else(|| {
                    if path.is_empty() {
                        return Some(self.layout_area());
                    }
                    let (&child_idx, parent_path) = path.split_last()?;
                    self.child_rect_at(parent_path, child_idx)
                })?,
            NodeData::Container(container) => container.geometry(),
        };

        Some(rect_to_ipc(rect, offset))
    }
}

fn layout_to_ipc(layout: Layout) -> LayoutTreeLayout {
    match layout {
        Layout::SplitH => LayoutTreeLayout::SplitH,
        Layout::SplitV => LayoutTreeLayout::SplitV,
        Layout::Tabbed => LayoutTreeLayout::Tabbed,
        Layout::Stacked => LayoutTreeLayout::Stacked,
    }
}

fn rect_to_ipc(rect: Rectangle<f64, Logical>, offset: Point<f64, Logical>) -> LayoutTreeRect {
    let loc = rect.loc + offset;
    LayoutTreeRect {
        x: loc.x,
        y: loc.y,
        width: rect.size.w,
        height: rect.size.h,
    }
}
