//! Marks attached to addressable container nodes.

use super::{ContainerArena, LayoutElement, NodeData, NodeKey};

impl<W: LayoutElement> ContainerArena<W> {
    pub(in crate::layout) fn node_marks(&self, key: NodeKey) -> Option<&[String]> {
        match self.get_node(key)? {
            NodeData::Container(container) => Some(container.marks()),
            NodeData::Workspace(_) => None,
        }
    }

    pub(in crate::layout) fn node_has_mark(&self, key: NodeKey, mark: &str) -> bool {
        self.get_any_container(key)
            .is_some_and(|container| container.has_mark(mark))
    }

    pub(in crate::layout) fn node_with_mark(&self, mark: &str) -> Option<NodeKey> {
        self.nodes.iter().find_map(|(key, node)| match node {
            NodeData::Container(container) if container.has_mark(mark) => Some(key),
            _ => None,
        })
    }

    pub(in crate::layout) fn add_mark_to_node(&mut self, key: NodeKey, mark: String) -> bool {
        let Some(container) = self.get_any_container_mut(key) else {
            return false;
        };
        container.add_mark(mark);
        true
    }

    pub(in crate::layout) fn remove_mark_from_node(&mut self, key: NodeKey, mark: &str) -> bool {
        let Some(container) = self.get_any_container_mut(key) else {
            return false;
        };
        container.remove_mark(mark);
        true
    }

    pub(in crate::layout) fn clear_marks_on_node(&mut self, key: NodeKey) -> bool {
        let Some(container) = self.get_any_container_mut(key) else {
            return false;
        };
        container.clear_marks();
        true
    }

    pub(in crate::layout) fn remove_mark_everywhere(&mut self, mark: &str) {
        for (_, node) in self.nodes.iter_mut() {
            if let NodeData::Container(container) = node {
                container.remove_mark(mark);
            }
        }
    }

    pub(in crate::layout) fn clear_marks_everywhere(&mut self) {
        for (_, node) in self.nodes.iter_mut() {
            if let NodeData::Container(container) = node {
                container.clear_marks();
            }
        }
    }

    pub(in crate::layout) fn representative_window_for_node(&self, key: NodeKey) -> Option<&W> {
        self.focused_window_in_subtree(key)
    }
}
