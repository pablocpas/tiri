//! Tab-bar model: per-container tab layout data for rendering and hit-testing.

use super::ContainerArena;
use super::Layout;
use super::LayoutElement;
use super::NodeData;
use super::NodeKey;
use super::TabBarInfo;
use super::TabBarTab;
use tiri_config::BlockOutFrom;

impl<W: LayoutElement> ContainerArena<W> {
    pub(in crate::layout) fn tab_bar_layouts(&self) -> Vec<TabBarInfo> {
        let mut out = Vec::new();
        let root_key = self.root;

        let mut path = Vec::new();
        self.collect_tab_bar_layouts(root_key, &mut path, &mut out, true);
        out
    }

    pub(super) fn collect_tab_bar_layouts(
        &self,
        node_key: NodeKey,
        path: &mut Vec<usize>,
        out: &mut Vec<TabBarInfo>,
        visible: bool,
    ) {
        let Some(container) = self.get_container(node_key) else {
            return;
        };

        if visible && matches!(container.layout, Layout::Tabbed | Layout::Stacked) {
            if let Some((rect, row_height)) = self.tab_bar_rect(
                container.layout,
                container.geometry,
                container.children.len(),
            ) {
                let focused_idx = self.active_child_index(node_key).unwrap_or(0);
                let tabs = container
                    .children
                    .iter()
                    .enumerate()
                    .map(|(idx, &child_key)| {
                        let (title, block_out_from) = self.focused_title_and_block_out(child_key);
                        TabBarTab {
                            title,
                            is_focused: idx == focused_idx,
                            is_urgent: self.subtree_has_urgent(child_key),
                            block_out_from,
                        }
                    })
                    .collect();

                out.push(TabBarInfo {
                    key: node_key,
                    path: path.clone(),
                    layout: container.layout,
                    rect,
                    row_height,
                    tabs,
                });
            }
        }

        let focused_idx = self.active_child_index(node_key).unwrap_or(0);
        for (idx, &child_key) in container.children.iter().enumerate() {
            path.push(idx);
            let child_visible = match container.layout {
                Layout::Tabbed | Layout::Stacked => idx == focused_idx,
                _ => true,
            };
            self.collect_tab_bar_layouts(child_key, path, out, visible && child_visible);
            path.pop();
        }
    }

    pub(in crate::layout) fn window_for_tab(
        &self,
        container_key: NodeKey,
        tab_idx: usize,
    ) -> Option<&W> {
        if let Some(container) = self.get_container(container_key) {
            let child_key = container.child_key(tab_idx)?;
            return self.focused_window_in_subtree(child_key);
        }

        if tab_idx == 0 {
            if let Some(tile) = self.get_node(container_key).and_then(|node| node.as_tile()) {
                return Some(tile.window());
            }
        }

        None
    }

    pub(super) fn focused_title_and_block_out(
        &self,
        node_key: NodeKey,
    ) -> (String, Option<BlockOutFrom>) {
        if let Some(window) = self.focused_window_in_subtree(node_key) {
            let title = window
                .title()
                .filter(|title| !title.trim().is_empty())
                .unwrap_or_else(|| String::from("untitled"));
            return (title, window.rules().block_out_from);
        }

        (String::from("untitled"), None)
    }

    pub(super) fn focused_window_in_subtree(&self, node_key: NodeKey) -> Option<&W> {
        match self.get_node(node_key) {
            Some(node) if node.is_view() => {
                let tile = node.as_tile().expect("a view holds a tile");
                Some(tile.window())
            }
            Some(NodeData::Workspace(_)) | Some(NodeData::Container(_)) => {
                let child_key = self.active_child(node_key)?;
                self.focused_window_in_subtree(child_key)
            }
            None => None,
        }
    }

    pub(super) fn subtree_has_urgent(&self, node_key: NodeKey) -> bool {
        match self.get_node(node_key) {
            Some(node) if node.is_view() => {
                let tile = node.as_tile().expect("a view holds a tile");
                tile.window().is_urgent()
            }
            Some(NodeData::Workspace(_)) | Some(NodeData::Container(_)) => self
                .get_container(node_key)
                .expect("layout parent")
                .children
                .iter()
                .any(|&child_key| self.subtree_has_urgent(child_key)),
            None => false,
        }
    }
}
