//! Window and subtree insertion at focus, path or split targets.

use super::ContainerData;
use super::ContainerTree;
use super::DetachedNode;
use super::Direction;
use super::InsertParentInfo;
use super::Layout;
use super::LayoutElement;
use super::NodeData;
use crate::layout::tile::Tile;

impl<W: LayoutElement> ContainerTree<W> {
    /// Insert a window into the tree, focusing it afterwards.
    #[cfg(test)]
    pub(in crate::layout) fn insert_window(&mut self, tile: Tile<W>) {
        self.insert_window_with_focus(tile, true);
    }

    /// Insert a window into the tree, optionally focusing it afterwards.
    pub(in crate::layout) fn insert_window_with_focus(&mut self, tile: Tile<W>, focus: bool) {
        self.clear_focus_history();

        if self.root.is_none() {
            let tile_key = self.insert_node(NodeData::Leaf(tile));

            if self.pending_layout_wrap_on_split {
                let layout = self.pending_layout.take().unwrap_or(Layout::SplitH);
                self.pending_layout_wrap_on_split = false;

                let mut container = ContainerData::new(layout);
                container.mark_preserve_on_single();
                container.add_child(tile_key);

                let container_key = self.insert_node(NodeData::Container(container));
                self.set_parent(tile_key, Some(container_key));
                self.set_parent(container_key, None);
                self.root = Some(container_key);
            } else {
                // Match i3/sway: layout commands issued on an empty workspace apply when a
                // second window arrives (root-leaf conversion), not to the first opened window.
                self.set_parent(tile_key, None);
                self.root = Some(tile_key);
            }

            if focus {
                self.focus_node_key(tile_key);
            } else if let Some(key) = self.focused_key {
                self.sync_container_focus_from_key(key);
            } else {
                self.focus_node_key(tile_key);
            }
            return;
        }

        // Ensure the root is a container so we can insert siblings easily
        let root_key = self.expect_root();
        if matches!(self.get_node(root_key), Some(NodeData::Leaf(_))) {
            // Convert the root leaf into a container
            let old_root_key = self.take_root();
            let layout = self.pending_layout.take().unwrap_or(Layout::SplitH);
            self.pending_layout_wrap_on_split = false;
            let mut container = ContainerData::new(layout);
            container.add_child(old_root_key);

            let container_key = self.insert_node(NodeData::Container(container));
            self.set_parent(old_root_key, Some(container_key));
            self.set_parent(container_key, None);
            self.root = Some(container_key);
            self.focus_node_key(old_root_key);
        }

        self.ensure_selected_root_has_parent_for_sibling_insert();

        // Prefer selected container/leaf target (focus-parent semantics),
        // then fall back to focused leaf.
        let selected_target =
            self.selected_key
                .and_then(|selected_key| match self.get_node(selected_key) {
                    // Match i3/sway semantics for focused containers:
                    // insert new windows as siblings of the selected container.
                    Some(NodeData::Container(_container)) => {
                        if let Some(parent_key) = self.parent_of(selected_key) {
                            let selected_idx = self.child_index(parent_key, selected_key)?;
                            Some((parent_key, selected_idx + 1))
                        } else {
                            None
                        }
                    }
                    Some(NodeData::Leaf(_)) => {
                        let parent_key = self.parent_of(selected_key)?;
                        let selected_idx = self.child_index(parent_key, selected_key)?;
                        Some((parent_key, selected_idx + 1))
                    }
                    None => None,
                });

        let focus_target =
            self.focused_key
                .or_else(|| self.first_leaf_key())
                .and_then(|focused_key| {
                    let parent_key = self.parent_of(focused_key)?;
                    let focused_idx = self.child_index(parent_key, focused_key)?;
                    Some((parent_key, focused_idx + 1))
                });

        let insert_target = selected_target.or(focus_target).or_else(|| {
            let root_key = self.root?;
            let root = self.get_container(root_key)?;
            Some((root_key, root.child_count()))
        });

        let tile_key = self.insert_node(NodeData::Leaf(tile));
        if let Some((parent_key, insert_idx)) = insert_target {
            let mut inserted = false;
            if let Some(NodeData::Container(parent_container)) = self.get_node_mut(parent_key) {
                parent_container.insert_child(insert_idx, tile_key);

                inserted = true;
            }
            if inserted {
                self.set_parent(tile_key, Some(parent_key));
                if focus {
                    self.focus_node_key(tile_key);
                } else if let Some(key) = self.focused_key {
                    self.sync_container_focus_from_key(key);
                } else {
                    self.focus_first_leaf();
                }
                return;
            }
        }

        // Fallback: append to root container
        if let Some(root_key) = self.root {
            let mut inserted = false;
            if let Some(NodeData::Container(container)) = self.get_node_mut(root_key) {
                let insert_idx = container.children.len();
                container.insert_child(insert_idx, tile_key);
                inserted = true;
            }
            if inserted {
                self.set_parent(tile_key, Some(root_key));
                if focus {
                    self.focus_node_key(tile_key);
                } else if let Some(key) = self.focused_key {
                    self.sync_container_focus_from_key(key);
                } else {
                    self.focus_first_leaf();
                }
            }
        }
    }

    /// Insert a detached subtree into the tree, optionally focusing it afterwards.
    pub(in crate::layout) fn insert_subtree_with_focus(
        &mut self,
        subtree: DetachedNode<W>,
        focus: bool,
    ) {
        self.clear_focus_history();

        let node_key = self.insert_subtree(subtree);

        let tree_has_no_leaves = self.first_leaf_key().is_none();
        if self.root.is_none() || tree_has_no_leaves {
            if let Some(old_root) = self.root.take() {
                self.remove_node_recursive(old_root);
            }
            self.set_parent(node_key, None);
            self.root = Some(node_key);
            if focus {
                self.focus_node_key(node_key);
            } else if let Some(key) = self.focused_key {
                self.sync_container_focus_from_key(key);
            } else {
                self.focus_node_key(node_key);
            }
            return;
        }

        // Ensure the root is a container so we can insert siblings easily.
        let root_key = self.expect_root();
        if matches!(self.get_node(root_key), Some(NodeData::Leaf(_))) {
            let old_root_key = self.take_root();
            let layout = self.pending_layout.take().unwrap_or(Layout::SplitH);
            self.pending_layout_wrap_on_split = false;
            let mut container = ContainerData::new(layout);
            container.add_child(old_root_key);

            let container_key = self.insert_node(NodeData::Container(container));
            self.set_parent(old_root_key, Some(container_key));
            self.set_parent(container_key, None);
            self.root = Some(container_key);
            self.focus_node_key(old_root_key);
        }

        self.ensure_selected_root_has_parent_for_sibling_insert();

        // Prefer selected container/leaf target (focus-parent semantics),
        // then fall back to focused leaf.
        let selected_target =
            self.selected_key
                .and_then(|selected_key| match self.get_node(selected_key) {
                    Some(NodeData::Container(_container)) => {
                        if let Some(parent_key) = self.parent_of(selected_key) {
                            let selected_idx = self.child_index(parent_key, selected_key)?;
                            Some((parent_key, selected_idx + 1))
                        } else {
                            None
                        }
                    }
                    Some(NodeData::Leaf(_)) => {
                        let parent_key = self.parent_of(selected_key)?;
                        let selected_idx = self.child_index(parent_key, selected_key)?;
                        Some((parent_key, selected_idx + 1))
                    }
                    None => None,
                });

        let focus_target =
            self.focused_key
                .or_else(|| self.first_leaf_key())
                .and_then(|focused_key| {
                    let parent_key = self.parent_of(focused_key)?;
                    let focused_idx = self.child_index(parent_key, focused_key)?;
                    Some((parent_key, focused_idx + 1))
                });

        let insert_target = selected_target.or(focus_target).or_else(|| {
            let root_key = self.root?;
            let root = self.get_container(root_key)?;
            Some((root_key, root.child_count()))
        });

        if let Some((parent_key, insert_idx)) = insert_target {
            let mut inserted = false;
            if let Some(NodeData::Container(parent_container)) = self.get_node_mut(parent_key) {
                parent_container.insert_child(insert_idx, node_key);
                inserted = true;
            }
            if inserted {
                self.set_parent(node_key, Some(parent_key));
                if focus {
                    self.focus_node_key(node_key);
                } else if let Some(key) = self.focused_key {
                    self.sync_container_focus_from_key(key);
                } else {
                    self.focus_first_leaf();
                }
                return;
            }
        }

        // Fallback: append to root container.
        if let Some(root_key) = self.root {
            let mut inserted = false;
            if let Some(NodeData::Container(container)) = self.get_node_mut(root_key) {
                let insert_idx = container.children.len();
                container.insert_child(insert_idx, node_key);
                inserted = true;
            }
            if inserted {
                self.set_parent(node_key, Some(root_key));
                if focus {
                    self.focus_node_key(node_key);
                } else if let Some(key) = self.focused_key {
                    self.sync_container_focus_from_key(key);
                } else {
                    self.focus_first_leaf();
                }
            }
        }
    }

    pub(in crate::layout) fn insert_leaf_after(
        &mut self,
        window_id: &W::Id,
        tile: Tile<W>,
        focus: bool,
    ) -> bool {
        let path = match self.find_window(window_id) {
            Some(path) => path,
            None => {
                self.append_leaf(tile, focus);
                return true;
            }
        };

        if path.is_empty() {
            self.append_leaf(tile, focus);
            return true;
        }

        let parent_path = &path[..path.len() - 1];
        let current_idx = *path.last().unwrap();

        let parent_key = if parent_path.is_empty() {
            self.root
        } else {
            self.get_node_key_at_path(parent_path)
        };

        if let Some(parent_key) = parent_key {
            let insert_idx = current_idx + 1;
            let tile_key = self.insert_node(NodeData::Leaf(tile));

            let mut inserted = false;
            if let Some(parent) = self.get_container_mut(parent_key) {
                parent.insert_child(insert_idx, tile_key);
                inserted = true;
            }
            if inserted {
                self.set_parent(tile_key, Some(parent_key));
                if focus {
                    self.focus_node_key(tile_key);
                } else if let Some(key) = self.focused_key {
                    self.sync_container_focus_from_key(key);
                } else {
                    self.focus_first_leaf();
                }
                return true;
            }
        }

        false
    }

    pub(in crate::layout) fn insert_leaf_in_root_container(
        &mut self,
        root_idx: usize,
        tile_idx: Option<usize>,
        tile: Tile<W>,
        focus: bool,
    ) -> bool {
        let root_key = self.ensure_root_container();

        let root_container = match self.get_container(root_key) {
            Some(c) => c,
            None => return false,
        };

        if root_idx >= root_container.children.len() {
            return false;
        }

        let Some(root_child_key) = root_container.child_key(root_idx) else {
            return false;
        };

        if matches!(self.get_node(root_child_key), Some(NodeData::Leaf(_))) {
            // Get the existing data first
            let (existing_key, existing_percent, focus_pos) =
                if let Some(container) = self.get_container_mut(root_key) {
                    let existing_key = container.children.remove(root_idx);
                    let existing_percent = container.child_percents.remove(root_idx);
                    let focus_pos = container
                        .focus_stack
                        .iter()
                        .position(|key| *key == existing_key);
                    container.focus_stack.retain(|key| *key != existing_key);
                    (existing_key, existing_percent, focus_pos)
                } else {
                    return false;
                };

            let mut root_child_container = ContainerData::new(Layout::SplitV);
            root_child_container.add_child(existing_key);
            let root_child_container_key =
                self.insert_node(NodeData::Container(root_child_container));
            self.set_parent(existing_key, Some(root_child_container_key));

            // Insert back
            if let Some(container) = self.get_container_mut(root_key) {
                container
                    .children
                    .insert(root_idx, root_child_container_key);
                container.child_percents.insert(root_idx, existing_percent);
                if let Some(pos) = focus_pos {
                    container.focus_stack.insert(pos, root_child_container_key);
                } else if !container.focus_stack.contains(&root_child_container_key) {
                    container.focus_stack.push(root_child_container_key);
                }
                container.ensure_focus_stack();
                container.normalize_child_percents();
            }
            self.set_parent(root_child_container_key, Some(root_key));
        }

        // Now insert the new tile
        let root_child_key = match self.get_container(root_key) {
            Some(c) => match c.child_key(root_idx) {
                Some(key) => key,
                None => return false,
            },
            None => return false,
        };
        let root_child_container = match self.get_container(root_child_key) {
            Some(c) => c,
            None => return false,
        };

        let insert_at = tile_idx.unwrap_or(root_child_container.children.len());
        let insert_at = insert_at.min(root_child_container.children.len());

        let tile_key = self.insert_node(NodeData::Leaf(tile));

        if let Some(root_child_container) = self.get_container_mut(root_child_key) {
            root_child_container.insert_child(insert_at, tile_key);

            if focus {
                self.focus_node_key(tile_key);
            } else if let Some(key) = self.focused_key {
                self.sync_container_focus_from_key(key);
            } else {
                self.focus_first_leaf();
            }
        }
        self.set_parent(tile_key, Some(root_child_key));

        true
    }

    pub(in crate::layout) fn insert_parent_info_for_window(
        &self,
        window_id: &W::Id,
    ) -> Option<InsertParentInfo> {
        let path = self.find_window(window_id)?;
        self.insert_parent_info_for_path(&path)
    }

    pub(super) fn insert_parent_info_for_path(&self, path: &[usize]) -> Option<InsertParentInfo> {
        if path.is_empty() {
            return None;
        }

        let mut parent_path = path.to_vec();
        let insert_idx = parent_path.pop().unwrap();
        let parent_key = if parent_path.is_empty() {
            self.root?
        } else {
            self.get_node_key_at_path(&parent_path)?
        };
        let parent = self.get_container(parent_key)?;
        Some(InsertParentInfo {
            parent_path,
            insert_idx,
            layout: parent.layout(),
            child_percents: parent.child_percents_slice().to_vec(),
        })
    }

    pub(in crate::layout) fn replace_leaf_at_path(
        &mut self,
        path: &[usize],
        tile: Tile<W>,
    ) -> Option<Tile<W>> {
        let key = self.get_node_key_at_path(path)?;
        match self.get_node_mut(key)? {
            NodeData::Leaf(existing) => Some(std::mem::replace(existing, tile)),
            _ => None,
        }
    }

    pub(in crate::layout) fn is_leaf_at_path(&self, path: &[usize]) -> bool {
        let Some(key) = self.get_node_key_at_path(path) else {
            return false;
        };
        matches!(self.get_node(key), Some(NodeData::Leaf(_)))
    }

    pub(in crate::layout) fn insert_leaf_with_parent_info(
        &mut self,
        info: &InsertParentInfo,
        tile: Tile<W>,
        focus: bool,
    ) -> bool {
        let container_key = match self.ensure_container_at_path(&info.parent_path, info.layout) {
            Some(key) => key,
            None => {
                self.append_leaf(tile, focus);
                return true;
            }
        };

        let tile_key = self.insert_node(NodeData::Leaf(tile));
        if let Some(container) = self.get_container_mut(container_key) {
            container.insert_child(info.insert_idx, tile_key);
            if info.child_percents.len() == container.child_percents.len() {
                container.child_percents = info.child_percents.clone();
                container.normalize_child_percents();
            }
        }
        self.set_parent(tile_key, Some(container_key));

        if focus {
            self.focus_node_key(tile_key);
        } else if let Some(key) = self.focused_key {
            self.sync_container_focus_from_key(key);
        } else {
            self.focus_first_leaf();
        }

        true
    }

    pub(in crate::layout) fn insert_subtree_with_parent_info(
        &mut self,
        info: &InsertParentInfo,
        subtree: DetachedNode<W>,
        focus: bool,
    ) -> bool {
        let container_key = match self.ensure_container_at_path(&info.parent_path, info.layout) {
            Some(key) => key,
            None => {
                self.insert_subtree_at_root(self.root_children_len(), subtree, focus);
                return true;
            }
        };

        let node_key = self.insert_subtree(subtree);
        if let Some(container) = self.get_container_mut(container_key) {
            container.insert_child(info.insert_idx, node_key);
            if info.child_percents.len() == container.child_percents.len() {
                container.child_percents = info.child_percents.clone();
                container.normalize_child_percents();
            }
        }
        self.set_parent(node_key, Some(container_key));

        if focus {
            self.focus_node_key(node_key);
        } else if let Some(key) = self.focused_key {
            self.sync_container_focus_from_key(key);
        } else {
            self.focus_first_leaf();
        }

        true
    }

    pub(in crate::layout) fn insert_leaf_split(
        &mut self,
        target_path: &[usize],
        direction: Direction,
        tile: Tile<W>,
        focus: bool,
    ) -> bool {
        if self.root.is_none() {
            self.append_leaf(tile, focus);
            return true;
        }

        let desired_layout = if direction.is_horizontal() {
            Layout::SplitH
        } else {
            Layout::SplitV
        };

        if target_path.is_empty() {
            let Some(root_key) = self.root else {
                self.append_leaf(tile, focus);
                return true;
            };
            if !matches!(self.get_node(root_key), Some(NodeData::Leaf(_))) {
                self.append_leaf(tile, focus);
                return true;
            }

            let tile_key = self.insert_node(NodeData::Leaf(tile));
            let mut container = ContainerData::new(desired_layout);
            container.mark_preserve_on_single();
            match direction {
                Direction::Left | Direction::Up => {
                    container.add_child(tile_key);
                    container.add_child(root_key);
                }
                Direction::Right | Direction::Down => {
                    container.add_child(root_key);
                    container.add_child(tile_key);
                }
            }

            let container_key = self.insert_node(NodeData::Container(container));
            self.set_parent(tile_key, Some(container_key));
            self.set_parent(root_key, Some(container_key));
            self.set_parent(container_key, None);
            self.root = Some(container_key);

            if focus {
                self.focus_node_key(tile_key);
            } else if let Some(key) = self.focused_key {
                self.sync_container_focus_from_key(key);
            } else {
                self.focus_first_leaf();
            }
            return true;
        }

        let parent_path = &target_path[..target_path.len() - 1];
        let target_idx = *target_path.last().unwrap();
        let parent_key = if parent_path.is_empty() {
            self.root
        } else {
            self.get_node_key_at_path(parent_path)
        };
        let Some(parent_key) = parent_key else {
            self.append_leaf(tile, focus);
            return true;
        };

        let parent = match self.get_container(parent_key) {
            Some(container) => container,
            None => {
                self.append_leaf(tile, focus);
                return true;
            }
        };
        let target_key = match parent.child_key(target_idx) {
            Some(key) => key,
            None => {
                self.append_leaf(tile, focus);
                return true;
            }
        };

        let parent_layout = parent.layout();
        if matches!(parent_layout, Layout::SplitH | Layout::SplitV)
            && parent_layout == desired_layout
        {
            let insert_idx = match direction {
                Direction::Left | Direction::Up => target_idx,
                Direction::Right | Direction::Down => target_idx + 1,
            };
            let tile_key = self.insert_node(NodeData::Leaf(tile));

            let container = self
                .get_container_mut(parent_key)
                .expect("insert split parent missing");
            container.insert_child(insert_idx, tile_key);

            self.set_parent(tile_key, Some(parent_key));
            if focus {
                self.focus_node_key(tile_key);
            } else if let Some(key) = self.focused_key {
                self.sync_container_focus_from_key(key);
            } else {
                self.focus_first_leaf();
            }
            return true;
        }

        let tile_key = self.insert_node(NodeData::Leaf(tile));
        let mut new_container = ContainerData::new(desired_layout);
        new_container.mark_preserve_on_single();
        match direction {
            Direction::Left | Direction::Up => {
                new_container.add_child(tile_key);
                new_container.add_child(target_key);
            }
            Direction::Right | Direction::Down => {
                new_container.add_child(target_key);
                new_container.add_child(tile_key);
            }
        }
        let new_container_key = self.insert_node(NodeData::Container(new_container));

        self.set_parent(tile_key, Some(new_container_key));
        self.set_parent(target_key, Some(new_container_key));

        let container = self
            .get_container_mut(parent_key)
            .expect("insert split parent missing");
        let idx = container
            .children
            .iter()
            .position(|child| *child == target_key)
            .expect("insert split target missing");
        container.children[idx] = new_container_key;

        if let Some(pos) = container
            .focus_stack
            .iter()
            .position(|key| *key == target_key)
        {
            container.focus_stack[pos] = new_container_key;
        } else if !container.focus_stack.contains(&new_container_key) {
            container.focus_stack.push(new_container_key);
        }
        container.ensure_focus_stack();

        self.set_parent(new_container_key, Some(parent_key));

        if focus {
            self.focus_node_key(tile_key);
        } else if let Some(key) = self.focused_key {
            self.sync_container_focus_from_key(key);
        } else {
            self.focus_first_leaf();
        }

        true
    }

    pub(in crate::layout) fn insert_leaf_split_root(
        &mut self,
        direction: Direction,
        tile: Tile<W>,
        focus: bool,
    ) -> bool {
        let desired_layout = if direction.is_horizontal() {
            Layout::SplitH
        } else {
            Layout::SplitV
        };

        if self.root.is_none() {
            self.append_leaf(tile, focus);
            return true;
        }

        let Some(root_key) = self.root else {
            self.append_leaf(tile, focus);
            return true;
        };

        let tile_key = self.insert_node(NodeData::Leaf(tile));

        if let Some(root_container) = self.get_container(root_key) {
            if root_container.layout() == desired_layout {
                let insert_idx = match direction {
                    Direction::Left | Direction::Up => 0,
                    Direction::Right | Direction::Down => root_container.child_count(),
                };
                if let Some(container) = self.get_container_mut(root_key) {
                    container.insert_child(insert_idx, tile_key);
                }

                self.set_parent(tile_key, Some(root_key));
                if focus {
                    self.focus_node_key(tile_key);
                } else if let Some(key) = self.focused_key {
                    self.sync_container_focus_from_key(key);
                } else {
                    self.focus_first_leaf();
                }
                return true;
            }
        }

        let mut new_container = ContainerData::new(desired_layout);
        new_container.mark_preserve_on_single();
        match direction {
            Direction::Left | Direction::Up => {
                new_container.add_child(tile_key);
                new_container.add_child(root_key);
            }
            Direction::Right | Direction::Down => {
                new_container.add_child(root_key);
                new_container.add_child(tile_key);
            }
        }
        let new_container_key = self.insert_node(NodeData::Container(new_container));

        self.set_parent(tile_key, Some(new_container_key));
        self.set_parent(root_key, Some(new_container_key));
        self.set_parent(new_container_key, None);
        self.root = Some(new_container_key);

        if focus {
            self.focus_node_key(tile_key);
        } else if let Some(key) = self.focused_key {
            self.sync_container_focus_from_key(key);
        } else {
            self.focus_first_leaf();
        }

        true
    }
}
