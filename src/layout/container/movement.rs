//! Moving nodes in a direction: sibling swaps, container entry/escape.

use super::ContainerData;
use super::ContainerTree;
use super::Direction;
use super::Layout;
use super::LayoutElement;
use super::NodeData;
use super::NodeKey;
#[cfg(test)]
use super::RootPolicy;
use super::TreeCommandTarget;

impl<W: LayoutElement> ContainerTree<W> {
    /// Move the current command target in a direction.
    #[cfg(test)]
    pub(in crate::layout) fn move_in_direction(&mut self, direction: Direction) -> bool {
        self.move_target_in_direction(
            direction,
            self.command_target(RootPolicy::MaterialContainer),
        )
    }

    /// Move an explicit command target in a direction.
    pub(in crate::layout) fn move_target_in_direction(
        &mut self,
        direction: Direction,
        target: TreeCommandTarget,
    ) -> bool {
        self.clear_focus_history();
        if self.root.is_none() {
            return false;
        }

        let (sync_key, mut move_path, preserve_selected_container) = match target {
            TreeCommandTarget::Workspace => return false,
            TreeCommandTarget::Container(key) => {
                if !matches!(self.get_node(key), Some(NodeData::Container(_))) {
                    return false;
                }
                let Some(path) = self.find_node_path(key) else {
                    return false;
                };
                (key, path, true)
            }
            TreeCommandTarget::Leaf(key) => {
                if !matches!(self.get_node(key), Some(NodeData::Leaf(_))) {
                    return false;
                }
                let Some(path) = self.find_node_path(key) else {
                    return false;
                };
                (key, path, false)
            }
        };

        self.sync_container_focus_from_key(sync_key);

        if move_path.is_empty() {
            return false;
        }

        loop {
            if move_path.is_empty() {
                break;
            }

            let parent_path = &move_path[..move_path.len() - 1];
            if parent_path.is_empty() {
                break;
            }

            let parent_key = match self.get_node_key_at_path(parent_path) {
                Some(key) => key,
                None => break,
            };
            let parent_container = match self.get_container(parent_key) {
                Some(container) => container,
                None => break,
            };

            if parent_container.child_count() == 1 && !parent_container.preserve_on_single() {
                move_path = parent_path.to_vec();
                continue;
            }
            break;
        }

        if move_path.is_empty() {
            return false;
        }

        let node_key = match self.get_node_key_at_path(&move_path) {
            Some(key) => key,
            None => return false,
        };
        let node_parent_path = &move_path[..move_path.len() - 1];
        let node_idx = *move_path.last().unwrap();

        let parent_key = if node_parent_path.is_empty() {
            self.root
        } else {
            self.get_node_key_at_path(node_parent_path)
        };

        let Some(parent_key) = parent_key else {
            return false;
        };

        let Some(parent_layout) = self.get_container(parent_key).map(|c| c.layout()) else {
            return false;
        };

        let layout_matches = parent_layout.is_parallel_to(direction);

        if layout_matches {
            let child_count = match self.get_container(parent_key) {
                Some(container) => container.child_count(),
                None => 0,
            };
            if child_count == 0 {
                return false;
            }

            let target_idx = match direction {
                Direction::Left | Direction::Up => {
                    if node_idx > 0 {
                        Some(node_idx - 1)
                    } else {
                        None
                    }
                }
                Direction::Right | Direction::Down => {
                    if node_idx + 1 < child_count {
                        Some(node_idx + 1)
                    } else {
                        None
                    }
                }
            };

            let Some(target_idx) = target_idx else {
                // At edge: escape to grandparent if possible.
                if node_parent_path.is_empty() {
                    return false;
                }
                let grandparent_path = &node_parent_path[..node_parent_path.len() - 1];
                let parent_idx = *node_parent_path.last().unwrap();
                let moved = self.move_node_to_grandparent(
                    node_key,
                    node_parent_path,
                    node_idx,
                    grandparent_path,
                    parent_idx,
                    direction,
                );
                if moved && preserve_selected_container {
                    self.selected_key = Some(node_key);
                }
                return moved;
            };

            let target_key = match self
                .get_container(parent_key)
                .and_then(|c| c.child_key(target_idx))
            {
                Some(key) => key,
                None => return false,
            };

            if matches!(parent_layout, Layout::SplitH | Layout::SplitV) {
                if let Some(target_container) = self.get_container(target_key) {
                    let should_enter = target_container.layout() != parent_layout
                        || target_container.preserve_on_single();
                    if should_enter {
                        let moved = self.move_node_into_container(
                            node_key,
                            node_parent_path,
                            node_idx,
                            target_key,
                            direction,
                            target_container.focused_child_index().unwrap_or(0),
                        );
                        if moved && preserve_selected_container {
                            self.selected_key = Some(node_key);
                        }
                        return moved;
                    }
                }
            }

            if let Some(container) = self.get_container_mut(parent_key) {
                container.children.swap(node_idx, target_idx);
                container.child_percents.swap(node_idx, target_idx);
            }

            self.focus_node_key(node_key);
            if preserve_selected_container {
                self.selected_key = Some(node_key);
            }
            return true;
        } else {
            let mut ancestor_path = node_parent_path.to_vec();
            while !ancestor_path.is_empty() {
                let ancestor_parent_path = &ancestor_path[..ancestor_path.len() - 1];
                let ancestor_idx = *ancestor_path.last().unwrap();
                let ancestor_parent_key = if ancestor_parent_path.is_empty() {
                    self.root
                } else {
                    self.get_node_key_at_path(ancestor_parent_path)
                };
                let Some(ancestor_parent_key) = ancestor_parent_key else {
                    break;
                };
                let Some(ancestor_parent_layout) =
                    self.get_container(ancestor_parent_key).map(|c| c.layout())
                else {
                    break;
                };

                let ancestor_parallel = ancestor_parent_layout.is_parallel_to(direction);
                if !ancestor_parallel {
                    ancestor_path.pop();
                    continue;
                }

                let ancestor_child_count = self
                    .get_container(ancestor_parent_key)
                    .map(|container| container.child_count())
                    .unwrap_or(0);
                let target_idx = match direction {
                    Direction::Left | Direction::Up => ancestor_idx.checked_sub(1),
                    Direction::Right | Direction::Down => {
                        (ancestor_idx + 1 < ancestor_child_count).then_some(ancestor_idx + 1)
                    }
                };
                let Some(target_idx) = target_idx else {
                    break;
                };
                let Some(target_key) = self
                    .get_container(ancestor_parent_key)
                    .and_then(|container| container.child_key(target_idx))
                else {
                    break;
                };
                if let Some(target_container) = self.get_container(target_key) {
                    let moved = self.move_node_into_container(
                        node_key,
                        node_parent_path,
                        node_idx,
                        target_key,
                        direction,
                        target_container.focused_child_index().unwrap_or(0),
                    );
                    if moved && preserve_selected_container {
                        self.selected_key = Some(node_key);
                    }
                    return moved;
                }

                break;
            }
        }

        if node_parent_path.is_empty() {
            let moved =
                self.move_root_node_orthogonally_into_adjacent(node_key, node_idx, direction);
            if moved && preserve_selected_container {
                self.selected_key = Some(node_key);
            }
            return moved;
        }

        let grandparent_path = &node_parent_path[..node_parent_path.len() - 1];
        let parent_idx = *node_parent_path.last().unwrap();

        let moved = self.move_node_to_grandparent(
            node_key,
            node_parent_path,
            node_idx,
            grandparent_path,
            parent_idx,
            direction,
        );
        if moved && preserve_selected_container {
            self.selected_key = Some(node_key);
        }
        moved
    }

    pub(super) fn move_node_to_grandparent(
        &mut self,
        node_key: NodeKey,
        node_parent_path: &[usize],
        node_idx: usize,
        grandparent_path: &[usize],
        parent_idx: usize,
        direction: Direction,
    ) -> bool {
        let node_parent_key = if node_parent_path.is_empty() {
            match self.root {
                Some(key) => key,
                None => return false,
            }
        } else {
            match self.get_node_key_at_path(node_parent_path) {
                Some(key) => key,
                None => return false,
            }
        };

        let parent_child_count = self
            .get_container(node_parent_key)
            .map(|container| container.child_count())
            .unwrap_or(0);
        let parent_will_be_removed = parent_child_count == 1;

        if let Some(container) = self.get_container_mut(node_parent_key) {
            let _ = container.remove_child(node_idx);
        } else {
            return false;
        }
        self.set_parent(node_key, None);

        let grandparent_key = if grandparent_path.is_empty() {
            match self.root {
                Some(key) => key,
                None => return false,
            }
        } else {
            match self.get_node_key_at_path(grandparent_path) {
                Some(key) => key,
                None => return false,
            }
        };

        let insert_at = match direction {
            Direction::Left | Direction::Up => {
                if parent_will_be_removed {
                    parent_idx.saturating_sub(1)
                } else {
                    parent_idx
                }
            }
            Direction::Right | Direction::Down => {
                if parent_will_be_removed {
                    parent_idx + 2
                } else {
                    parent_idx + 1
                }
            }
        };

        if let Some(container) = self.get_container_mut(grandparent_key) {
            container.insert_child(insert_at, node_key);
        } else {
            return false;
        }
        self.set_parent(node_key, Some(grandparent_key));

        self.cleanup_containers(Some(node_parent_key));

        self.focus_node_key(node_key);

        true
    }

    pub(super) fn move_node_into_container(
        &mut self,
        node_key: NodeKey,
        node_parent_path: &[usize],
        node_idx: usize,
        target_key: NodeKey,
        direction: Direction,
        target_focus_idx: usize,
    ) -> bool {
        let (insert_idx, child_count) = if let Some(container) = self.get_container(target_key) {
            let child_count = container.child_count();
            let insert_idx = match container.layout() {
                Layout::SplitH | Layout::SplitV => {
                    let axis_matches = (container.layout() == Layout::SplitH
                        && direction.is_horizontal())
                        || (container.layout() == Layout::SplitV && direction.is_vertical());
                    if axis_matches {
                        match direction {
                            Direction::Left | Direction::Up => child_count,
                            Direction::Right | Direction::Down => 0,
                        }
                    } else {
                        match direction {
                            Direction::Left | Direction::Up => target_focus_idx + 1,
                            Direction::Right | Direction::Down => target_focus_idx,
                        }
                    }
                }
                Layout::Tabbed | Layout::Stacked => match direction {
                    Direction::Left | Direction::Up => target_focus_idx,
                    Direction::Right | Direction::Down => target_focus_idx + 1,
                },
            };
            (insert_idx, child_count)
        } else {
            return false;
        };

        let node_parent_key = if node_parent_path.is_empty() {
            match self.root {
                Some(key) => key,
                None => return false,
            }
        } else {
            match self.get_node_key_at_path(node_parent_path) {
                Some(key) => key,
                None => return false,
            }
        };

        if let Some(container) = self.get_container_mut(node_parent_key) {
            let _ = container.remove_child(node_idx);
        } else {
            return false;
        }

        if let Some(container) = self.get_container_mut(target_key) {
            let idx = insert_idx.min(child_count);
            container.insert_child(idx, node_key);
        } else {
            return false;
        }
        self.set_parent(node_key, Some(target_key));

        self.cleanup_containers(Some(node_parent_key));

        self.focus_node_key(node_key);

        true
    }

    pub(super) fn wrap_child_in_container(
        &mut self,
        parent_key: NodeKey,
        child_idx: usize,
        layout: Layout,
    ) -> Option<NodeKey> {
        let child_key = self.get_container(parent_key)?.child_key(child_idx)?;
        if matches!(self.get_node(child_key), Some(NodeData::Container(_))) {
            return Some(child_key);
        }

        let _ = self.get_container_mut(parent_key)?.remove_child(child_idx);
        self.set_parent(child_key, None);

        let mut wrapper = ContainerData::new(layout);
        wrapper.add_child(child_key);
        let wrapper_key = self.insert_node(NodeData::Container(wrapper));
        self.set_parent(child_key, Some(wrapper_key));

        self.get_container_mut(parent_key)?
            .insert_child(child_idx, wrapper_key);
        self.set_parent(wrapper_key, Some(parent_key));

        Some(wrapper_key)
    }

    pub(super) fn move_root_node_orthogonally_into_adjacent(
        &mut self,
        node_key: NodeKey,
        node_idx: usize,
        direction: Direction,
    ) -> bool {
        let Some(root_key) = self.root else {
            return false;
        };

        let child_count = self
            .get_container(root_key)
            .map(|container| container.child_count())
            .unwrap_or(0);
        if child_count <= 1 {
            return false;
        }

        let target_idx = match direction {
            Direction::Left | Direction::Up => node_idx.checked_sub(1),
            Direction::Right | Direction::Down => {
                (node_idx + 1 < child_count).then_some(node_idx + 1)
            }
        };
        let Some(target_idx) = target_idx else {
            return false;
        };

        let wrapped_target = !matches!(
            self.get_container(root_key)
                .and_then(|container| container.child_key(target_idx))
                .and_then(|key| self.get_node(key)),
            Some(NodeData::Container(_))
        );
        let desired_layout = if direction.is_horizontal() {
            Layout::SplitH
        } else {
            Layout::SplitV
        };
        let Some(target_key) = self.wrap_child_in_container(root_key, target_idx, desired_layout)
        else {
            return false;
        };
        let target_focus_idx = self
            .get_container(target_key)
            .and_then(|container| container.focused_child_index())
            .unwrap_or(0);

        let moved = self.move_node_into_container(
            node_key,
            &[],
            node_idx,
            target_key,
            direction,
            target_focus_idx,
        );
        if moved && wrapped_target {
            let _ = self.promote_single_root_child();
        }
        moved
    }

    pub(super) fn promote_single_root_child(&mut self) -> bool {
        let Some(root_key) = self.root else {
            return false;
        };
        let Some(root) = self.get_container(root_key) else {
            return false;
        };
        if root.child_count() != 1 {
            return false;
        }
        let Some(child_key) = root.children().first().copied() else {
            return false;
        };

        self.set_parent(child_key, None);
        self.root = Some(child_key);
        self.nodes.remove(root_key);
        self.parents.remove(root_key);

        if self.selected_key == Some(root_key) {
            self.selected_key = Some(child_key);
        }
        if self.focused_key == Some(root_key) {
            self.focused_key = self.leaf_under_key(child_key).or(Some(child_key));
        }

        true
    }
}
