use smithay::utils::{Logical, Rectangle};

use super::{ContainerTree, Layout, NodeData, NodeKey};
use crate::layout::LayoutElement;
use tiri_ipc::{LayoutTreeLayout, LayoutTreeNode, LayoutTreeRect};

impl<W: LayoutElement> ContainerTree<W> {
    pub(in crate::layout) fn layout_tree(&self) -> Option<LayoutTreeNode> {
        let root_key = self.root_node_key()?;
        let focused_key = self.selected_node_key();
        Some(self.build_layout_tree_node(root_key, focused_key, &mut Vec::new(), None, false))
    }

    pub(in crate::layout) fn layout_tree_unfocused(&self) -> Option<LayoutTreeNode> {
        let root_key = self.root_node_key()?;
        Some(self.build_layout_tree_node(root_key, None, &mut Vec::new(), None, false))
    }

    /// One branch as an IPC subtree, addressed from its own root.
    ///
    /// The rectangles are already the workspace's: one arrange pass lays both sides out in
    /// absolute coordinates, so a floating group needs no offset applied on the way out.
    pub(in crate::layout) fn layout_tree_for_branch(
        &self,
        branch_root: NodeKey,
        focused_key: Option<NodeKey>,
        path: &mut Vec<usize>,
        is_floating: bool,
    ) -> Option<LayoutTreeNode> {
        self.get_node(branch_root)?;
        Some(self.build_layout_tree_node(branch_root, focused_key, path, None, is_floating))
    }

    fn build_layout_tree_node(
        &self,
        node_key: NodeKey,
        focused_key: Option<NodeKey>,
        path: &mut Vec<usize>,
        percent: Option<f64>,
        is_floating: bool,
    ) -> LayoutTreeNode {
        match self.get_node(node_key) {
            Some(NodeData::Leaf(tile)) => {
                let window = tile.window();

                LayoutTreeNode {
                    path: path.clone(),
                    layout: None,
                    window_id: Some(window.ipc_id()),
                    title: window.title(),
                    app_id: window.app_id(),
                    pid: window.pid(),
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
                    rect: self.node_rect(node_key),
                    percent,
                    children: Vec::new(),
                }
            }
            Some(NodeData::Container(container)) => {
                let child_count = container.child_count();
                let percents = self.get_normalized_child_percents(node_key, child_count);
                let mut children = Vec::with_capacity(child_count);

                for (idx, child_key) in container.children().iter().enumerate() {
                    path.push(idx);
                    children.push(self.build_layout_tree_node(
                        *child_key,
                        focused_key,
                        path,
                        percents.get(idx).copied(),
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
                    rect: self.node_rect(node_key),
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

    fn node_rect(&self, node_key: NodeKey) -> Option<LayoutTreeRect> {
        // Both kinds answer with the box they are holding, and a node that has never been
        // arranged is holding none. sway's `container_create` zeroes `pending` and only
        // `arrange` ever fills it in, so a container built while a fullscreen is up — or a
        // window opened into one — reports 0x0 for as long as that lasts. Working the
        // rectangle out from the parent instead answers where the node *will* be, which is a
        // different question and one nothing asked.
        let rect = match self.get_node(node_key)? {
            NodeData::Leaf(_) => self
                .leaf_layouts()
                .iter()
                .find(|info| info.key == node_key)
                .map(|info| info.rect)
                .unwrap_or_default(),
            NodeData::Container(container) => container.geometry(),
        };

        Some(rect_to_ipc(rect))
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

fn rect_to_ipc(rect: Rectangle<f64, Logical>) -> LayoutTreeRect {
    let loc = rect.loc;
    LayoutTreeRect {
        x: loc.x,
        y: loc.y,
        width: rect.size.w,
        height: rect.size.h,
    }
}
