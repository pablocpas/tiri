//! Tree normalization: collapsing redundant containers after mutations.

use super::ContainerData;
use super::ContainerTree;
use super::Layout;
use super::LayoutElement;
use super::NodeData;
use super::NodeKey;

impl<W: LayoutElement> ContainerTree<W> {
    pub(super) fn cleanup_containers(&mut self, mut key: Option<NodeKey>) {
        loop {
            let Some(container_key) = key else {
                if let Some(root_key) = self.root {
                    if let Some(container) = self.get_container(root_key) {
                        if container.children.is_empty() {
                            self.pending_layout = None;
                            self.pending_layout_wrap_on_split = false;
                            self.remove_node_recursive(root_key);
                            self.root = None;
                        }
                    }
                }
                break;
            };

            let parent_key = self.parent_of(container_key);
            let Some(container) = self.get_container(container_key) else {
                key = parent_key;
                continue;
            };

            let container_layout = container.layout();
            let container_children = container.children.clone();
            let container_focus_stack = container.focus_stack.clone();
            let container_child_percents = container.child_percents_slice().to_vec();
            let container_preserve_on_single = container.preserve_on_single();
            let child_count = container_children.len();

            let mut remove_container = false;
            let mut replace_with_child = None;
            let mut squash_with_parent = false;

            let parent_layout = parent_key.and_then(|parent_key| {
                self.get_container(parent_key).map(|parent| parent.layout())
            });

            let single_child_key = container_children.first().copied();
            let can_replace_with_child = !container_preserve_on_single
                && match parent_key {
                    Some(_) => true,
                    None => single_child_key.is_some_and(|child_key| {
                        matches!(self.get_node(child_key), Some(NodeData::Leaf(_)))
                    }),
                };

            if child_count == 0 {
                remove_container = true;
            } else if child_count == 1 && can_replace_with_child {
                replace_with_child = container_children.first().copied();
            } else if child_count > 1
                && !container_preserve_on_single
                && parent_layout
                    .map(|layout| Self::layouts_squashable(layout, container_layout))
                    .unwrap_or(false)
            {
                squash_with_parent = true;
            }

            if let Some(parent_key) = parent_key {
                let parent_idx = match self.child_index(parent_key, container_key) {
                    Some(idx) => idx,
                    None => {
                        key = Some(parent_key);
                        continue;
                    }
                };

                if squash_with_parent {
                    let (parent_children, parent_focus, parent_percents) =
                        if let Some(parent) = self.get_container(parent_key) {
                            (
                                parent.children.clone(),
                                parent.focus_stack.clone(),
                                parent.child_percents_slice().to_vec(),
                            )
                        } else {
                            key = Some(parent_key);
                            continue;
                        };

                    let mut new_children = Vec::with_capacity(
                        parent_children.len().saturating_sub(1) + container_children.len(),
                    );
                    new_children.extend_from_slice(&parent_children[..parent_idx]);
                    new_children.extend_from_slice(&container_children);
                    new_children.extend_from_slice(&parent_children[parent_idx + 1..]);

                    let mut new_focus = Vec::with_capacity(
                        parent_focus.len().saturating_sub(1) + container_focus_stack.len(),
                    );
                    for key in parent_focus {
                        if key == container_key {
                            for child in &container_focus_stack {
                                if container_children.contains(child) && !new_focus.contains(child)
                                {
                                    new_focus.push(*child);
                                }
                            }
                        } else if !new_focus.contains(&key) {
                            new_focus.push(key);
                        }
                    }
                    for child in &container_children {
                        if !new_focus.contains(child) {
                            new_focus.push(*child);
                        }
                    }

                    let mut new_percents = Vec::new();
                    if parent_percents.len() == parent_children.len() {
                        let replaced_share = parent_percents[parent_idx];
                        new_percents.extend_from_slice(&parent_percents[..parent_idx]);

                        if !container_children.is_empty() {
                            if container_child_percents.len() == container_children.len() {
                                let sum: f64 = container_child_percents.iter().copied().sum();
                                if sum > f64::EPSILON {
                                    for percent in &container_child_percents {
                                        new_percents.push(replaced_share * (*percent / sum));
                                    }
                                } else {
                                    let value = replaced_share / container_children.len() as f64;
                                    new_percents.resize(
                                        new_percents.len() + container_children.len(),
                                        value,
                                    );
                                }
                            } else {
                                let value = replaced_share / container_children.len() as f64;
                                new_percents
                                    .resize(new_percents.len() + container_children.len(), value);
                            }
                        }

                        new_percents.extend_from_slice(&parent_percents[parent_idx + 1..]);
                    }

                    if let Some(parent) = self.get_container_mut(parent_key) {
                        parent.children = new_children;
                        parent.focus_stack = new_focus;
                        if new_percents.len() == parent.children.len() {
                            parent.child_percents = new_percents;
                            parent.normalize_child_percents();
                        } else {
                            parent.recalculate_percentages();
                        }
                        parent.ensure_focus_stack();
                    }

                    for child_key in &container_children {
                        self.set_parent(*child_key, Some(parent_key));
                    }

                    if self.selected_key == Some(container_key) {
                        self.selected_key = container_focus_stack
                            .iter()
                            .copied()
                            .find(|child| container_children.contains(child))
                            .or_else(|| container_children.first().copied())
                            .or(Some(parent_key));
                    }

                    if self.focused_key == Some(container_key) {
                        self.focused_key = container_focus_stack
                            .iter()
                            .copied()
                            .find(|child| container_children.contains(child))
                            .or_else(|| container_children.first().copied())
                            .or(Some(parent_key));
                    }

                    self.nodes.remove(container_key);
                    self.parents.remove(container_key);
                } else if remove_container {
                    if let Some(parent) = self.get_container_mut(parent_key) {
                        parent.remove_child(parent_idx);
                    }
                    self.set_parent(container_key, None);
                    self.remove_node_recursive(container_key);
                } else if let Some(child_key) = replace_with_child {
                    if let Some(parent) = self.get_container_mut(parent_key) {
                        parent.children[parent_idx] = child_key;
                        if let Some(pos) = parent
                            .focus_stack
                            .iter()
                            .position(|key| *key == container_key)
                        {
                            parent.focus_stack[pos] = child_key;
                        } else if !parent.focus_stack.contains(&child_key) {
                            parent.focus_stack.push(child_key);
                        }
                        parent.ensure_focus_stack();
                    }
                    self.set_parent(child_key, Some(parent_key));
                    self.nodes.remove(container_key);
                    self.parents.remove(container_key);
                }
            } else if remove_container {
                self.pending_layout = Some(container_layout);
                self.pending_layout_wrap_on_split = container_preserve_on_single;
                self.remove_node_recursive(container_key);
                self.root = None;
            } else if let Some(child_key) = replace_with_child {
                if self.selected_key == Some(container_key) {
                    self.selected_key = Some(child_key);
                }
                if self.focused_key == Some(container_key) {
                    self.focused_key = Some(child_key);
                }
                self.set_parent(child_key, None);
                self.nodes.remove(container_key);
                self.parents.remove(container_key);
                self.root = Some(child_key);
            }

            key = parent_key;
        }

        while let Some(root_key) = self.root {
            let Some(root) = self.get_container(root_key) else {
                break;
            };
            if root.child_count() != 1 || root.preserve_on_single() {
                break;
            }

            let Some(child_key) = root.child_key(0) else {
                break;
            };
            if self
                .get_container(child_key)
                .is_some_and(|child| child.child_count() > 1)
            {
                break;
            }
            if self.selected_key == Some(root_key) {
                self.selected_key = Some(child_key);
            }
            if self.focused_key == Some(root_key) {
                self.focused_key = Some(child_key);
            }

            self.set_parent(child_key, None);
            self.nodes.remove(root_key);
            self.parents.remove(root_key);
            self.root = Some(child_key);
        }
    }

    pub(super) fn ensure_root_container(&mut self) -> NodeKey {
        if self.root.is_none() {
            let explicit_layout = self.pending_layout.is_some();
            let layout = self.pending_layout.take().unwrap_or(Layout::SplitH);
            self.pending_layout_wrap_on_split = false;
            let mut container = ContainerData::new(layout);
            if explicit_layout {
                container.mark_preserve_on_single();
            }
            let container_key = self.insert_node(NodeData::Container(container));
            self.set_parent(container_key, None);
            self.root = Some(container_key);
            self.focused_key = None;
            return container_key;
        }

        let root_key = self.expect_root();
        let needs_conversion = matches!(self.get_node(root_key), Some(NodeData::Leaf(_)));

        if needs_conversion {
            let old_root_key = self.take_root();
            let mut container = ContainerData::new(Layout::SplitH);
            container.add_child(old_root_key);
            let container_key = self.insert_node(NodeData::Container(container));
            self.set_parent(old_root_key, Some(container_key));
            self.set_parent(container_key, None);
            self.root = Some(container_key);
            self.focus_node_key(old_root_key);
            container_key
        } else {
            root_key
        }
    }

    pub(super) fn ensure_container_at_path(
        &mut self,
        path: &[usize],
        layout: Layout,
    ) -> Option<NodeKey> {
        let root_key = self.root?;
        if path.is_empty() {
            if matches!(self.get_node(root_key), Some(NodeData::Container(_))) {
                return Some(root_key);
            }

            let mut container = ContainerData::new(layout);
            container.mark_preserve_on_single();
            container.add_child(root_key);
            let container_key = self.insert_node(NodeData::Container(container));
            self.set_parent(root_key, Some(container_key));
            self.set_parent(container_key, None);
            self.root = Some(container_key);
            return Some(container_key);
        }

        let key = self.get_node_key_at_path(path)?;
        if matches!(self.get_node(key), Some(NodeData::Container(_))) {
            return Some(key);
        }

        let parent_path = &path[..path.len() - 1];
        let parent_key = if parent_path.is_empty() {
            self.root?
        } else {
            self.get_node_key_at_path(parent_path)?
        };
        let child_idx = *path.last().unwrap();

        let mut container = ContainerData::new(layout);
        container.mark_preserve_on_single();
        container.add_child(key);
        let container_key = self.insert_node(NodeData::Container(container));
        self.set_parent(key, Some(container_key));

        if let Some(parent) = self.get_container_mut(parent_key) {
            parent.children[child_idx] = container_key;
            if let Some(pos) = parent.focus_stack.iter().position(|k| *k == key) {
                parent.focus_stack[pos] = container_key;
            } else if !parent.focus_stack.contains(&container_key) {
                parent.focus_stack.push(container_key);
            }
            parent.ensure_focus_stack();
        }

        self.set_parent(container_key, Some(parent_key));
        Some(container_key)
    }
}
