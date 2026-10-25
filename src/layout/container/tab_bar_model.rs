//! Tab-bar model: per-container tab layout data for rendering and hit-testing.

use tiri_config::BlockOutFrom;

use super::{
    ActiveChildren, ContainerTree, LayoutElement, LayoutMode, NodeData, NodeKey, TabBarInfo,
    TabBarTab,
};

impl<W: LayoutElement> ContainerTree<W> {
    pub(in crate::layout) fn tab_bar_layouts(&self, workspace: NodeKey) -> Vec<TabBarInfo> {
        let mut out = Vec::new();

        self.collect_tab_bar_layouts(&self.active_children(), workspace, &mut out, true);
        out
    }

    pub(super) fn collect_tab_bar_layouts(
        &self,
        active: &ActiveChildren,
        node_key: NodeKey,
        out: &mut Vec<TabBarInfo>,
        visible: bool,
    ) {
        let Some(container) = self.get_container(node_key) else {
            return;
        };

        let focused_idx = active
            .of(self, node_key)
            .and_then(|key| self.child_index(node_key, key))
            .unwrap_or(0);
        if visible && matches!(container.layout, LayoutMode::Tabbed | LayoutMode::Stacked) {
            let workspace = self
                .workspace_of(node_key)
                .expect("a tabbed container belongs to a workspace");
            if let Some((rect, row_height)) = self.tab_bar_rect(
                workspace,
                container.layout,
                container.geometry,
                container.children.len(),
            ) {
                let focused_key = self.seat_focused_key();
                let tabs = container
                    .children
                    .iter()
                    .enumerate()
                    .map(|(idx, &child_key)| {
                        let (title, block_out_from) =
                            self.focused_title_and_block_out(active, child_key);
                        TabBarTab {
                            title,
                            is_focused: idx == focused_idx,
                            holds_focus: focused_key
                                .is_some_and(|key| self.is_descendant(key, child_key)),
                            is_urgent: self.subtree_has_urgent(child_key),
                            block_out_from,
                        }
                    })
                    .collect();

                out.push(TabBarInfo {
                    key: node_key,
                    layout: container.layout,
                    rect,
                    row_height,
                    tabs,
                });
            }
        }

        for (idx, &child_key) in container.children.iter().enumerate() {
            let child_visible = match container.layout {
                LayoutMode::Tabbed | LayoutMode::Stacked => idx == focused_idx,
                _ => true,
            };
            self.collect_tab_bar_layouts(active, child_key, out, visible && child_visible);
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
        active: &ActiveChildren,
        node_key: NodeKey,
    ) -> (String, Option<BlockOutFrom>) {
        if let Some(window) = self.focused_window_in_subtree_with(active, node_key) {
            let title = window
                .title()
                .filter(|title| !title.trim().is_empty())
                .unwrap_or_else(|| String::from("untitled"));
            return (title, window.rules().block_out_from);
        }

        (String::from("untitled"), None)
    }

    pub(super) fn focused_window_in_subtree(&self, node_key: NodeKey) -> Option<&W> {
        self.focused_window_in_subtree_with(&self.active_children(), node_key)
    }

    fn focused_window_in_subtree_with(
        &self,
        active: &ActiveChildren,
        node_key: NodeKey,
    ) -> Option<&W> {
        match self.get_node(node_key) {
            Some(node) if node.is_view() => {
                let tile = node.as_tile().expect("a view holds a tile");
                Some(tile.window())
            }
            Some(NodeData::Workspace(_)) | Some(NodeData::Container(_)) => {
                let child_key = active.of(self, node_key)?;
                self.focused_window_in_subtree_with(active, child_key)
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
