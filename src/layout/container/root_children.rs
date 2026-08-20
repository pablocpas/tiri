//! Root-child ("column") compatibility layer over the tree root.

use super::ContainerArena;
use super::ContainerData;
use super::DetachedNode;
use super::LayoutElement;
use super::NodeData;
use super::NodeKey;
use super::WorkspaceData;
use crate::layout::tile::Tile;

impl<W: LayoutElement> ContainerArena<W> {
    /// Number of root-level children (columns).
    pub(in crate::layout) fn root_children_len(&self) -> usize {
        let root_key = self.root;

        match self.get_node(root_key) {
            Some(node) if node.is_view() => 1,
            Some(NodeData::Workspace(workspace)) => workspace.children.len(),
            Some(NodeData::Container(container)) => container.children.len(),
            None => 0,
        }
    }

    pub(in crate::layout) fn root_container(&self) -> Option<&WorkspaceData> {
        self.get_workspace()
    }

    /// Index of currently focused root child, if any.
    pub(in crate::layout) fn focused_root_index(&self) -> Option<usize> {
        let root_key = self.root;
        if let Some(key) = self.focused_key() {
            if key == root_key {
                return Some(0);
            }
            let mut child = key;
            let mut parent = self.parent_of(child)?;
            while parent != root_key {
                child = parent;
                parent = self.parent_of(child)?;
            }
            return self.child_index(root_key, child);
        }

        match self.get_node(root_key) {
            Some(node) if node.is_view() => Some(0),
            Some(NodeData::Workspace(_)) | Some(NodeData::Container(_)) => {
                // Nothing focused yet: fall back to the container's own focus history.
                let Some(focused_key) = self.effective_focused_key() else {
                    return self.active_child_index(root_key);
                };
                // Walk up from the focused leaf to the root child that holds it.
                let mut child = focused_key;
                while let Some(parent) = self.parent_of(child) {
                    if parent == root_key {
                        return self.child_index(root_key, child);
                    }
                    child = parent;
                }
                self.active_child_index(root_key)
            }
            None => None,
        }
    }

    /// Focus root child at index, descending to the first leaf.
    pub(in crate::layout) fn focus_root_child(&mut self, idx: usize) -> bool {
        self.clear_focus_history();
        let root_key = self.root;

        match self.get_node(root_key) {
            Some(node) if node.is_view() => {
                if idx == 0 {
                    self.focus_node_key(root_key);
                    true
                } else {
                    false
                }
            }
            Some(NodeData::Workspace(workspace)) => {
                if idx >= workspace.children.len() {
                    return false;
                }
                let child_key = workspace.child_key(idx);
                if let Some(child_key) = child_key {
                    self.focus_node_key(child_key);
                    true
                } else {
                    false
                }
            }
            Some(NodeData::Container(container)) => {
                if idx >= container.children.len() {
                    return false;
                }
                let child_key = container.child_key(idx);
                if let Some(child_key) = child_key {
                    self.focus_node_key(child_key);
                    true
                } else {
                    false
                }
            }
            None => false,
        }
    }

    /// Move a root child from one index to another
    pub(in crate::layout) fn move_root_child(&mut self, from: usize, to: usize) -> bool {
        self.clear_focus_history();
        let root_key = self.root;

        let container = match self.get_container_mut(root_key) {
            Some(c) => c,
            None => return false,
        };

        if from >= container.children.len() || to >= container.children.len() {
            return false;
        }

        let node_key = container.children.remove(from);
        container.children.insert(to, node_key);

        self.resync_focus();
        true
    }

    /// Insert a detached subtree at root level.
    pub(in crate::layout) fn insert_subtree_at_root(
        &mut self,
        index: usize,
        subtree: DetachedNode<W>,
        focus: bool,
    ) {
        // A pending arrange of the destination predates this entire branch. Waiting for it
        // would leave the transferred container (including its tab bar) absent until another
        // commit or command invalidated the layout.
        self.discard_layout_superseded_by_transfer();
        let node_key = self.insert_subtree(subtree);
        if self.unwrap_into_empty_workspace(node_key, focus) {
            return;
        }
        self.insert_key_at_root(index, node_key, focus);
    }

    /// A container arriving at an empty workspace becomes the workspace.
    ///
    /// sway's `container_move_to_workspace` (`sway/commands/move.c:222`) branches on exactly
    /// this: an empty destination and a node with children take
    /// `workspace_unwrap_children` followed by `container_reap_empty`, so the workspace
    /// adopts the container's layout and its children, and the container itself is gone.
    /// Anything else is added as a child with its shares cleared, because a fraction is
    /// relative to a parent the node has left.
    ///
    /// Without it a workspace that has never held anything comes out one level deeper than
    /// the one the container was on, which is a difference in the tree the user can see the
    /// moment they split something there.
    fn unwrap_into_empty_workspace(&mut self, node_key: NodeKey, focus: bool) -> bool {
        let root = self.root;
        if self
            .get_container(root)
            .is_none_or(|workspace| workspace.child_count() != 0)
        {
            return false;
        }
        let Some(container) = self.get_real_container(node_key) else {
            return false;
        };
        if container.child_count() == 0 {
            return false;
        }

        let layout = container.layout();
        let children: Vec<NodeKey> = container.children().to_vec();
        if let Some(workspace) = self.get_container_mut(root) {
            workspace.set_layout(layout);
        }
        for (idx, child) in children.iter().enumerate() {
            if let Some(workspace) = self.get_container_mut(root) {
                workspace.insert_child(idx, *child);
            }
            self.set_parent(*child, Some(root));
        }
        // Its children have already left, so the wrapper goes on its own — this is
        // `container_reap_empty` on a container nothing is left inside.
        if let Some(container) = self.get_container_mut(node_key) {
            while container.remove_child(0).is_some() {}
        }
        self.remove_node_from_store(node_key);
        self.prune_focus_order();

        // The seat has to be told where the focus went: the node it was pointing at no
        // longer exists, and the children arrived without anything focusing them.
        if let Some(&first) = children.first() {
            self.settle_focus_after_insert(first, focus);
        }
        true
    }

    /// Focus nth (1-based) leaf within the given root child.
    pub(in crate::layout) fn focus_leaf_in_root_child(
        &mut self,
        child_idx: usize,
        leaf_idx: usize,
    ) -> bool {
        self.clear_focus_history();
        if leaf_idx == 0 {
            return false;
        }
        let Some(child_key) = self
            .get_container(self.root)
            .and_then(|root| root.child_key(child_idx))
        else {
            return false;
        };
        let leaves = self.leaf_keys_under(child_key);
        let Some(&key) = leaves.get(leaf_idx - 1) else {
            return false;
        };
        self.focus_node_key(key);
        true
    }

    /// Focus the first leaf under the focused root child, whatever its layout.
    pub(in crate::layout) fn focus_first_leaf_in_focused_root_child(&mut self) -> bool {
        let idx = match self.focused_root_index() {
            Some(idx) => idx,
            None => return false,
        };
        self.focus_leaf_in_root_child(idx, 1)
    }

    /// Focus the last leaf under the focused root child, whatever its layout.
    pub(in crate::layout) fn focus_last_leaf_in_focused_root_child(&mut self) -> bool {
        let idx = match self.focused_root_index() {
            Some(idx) => idx,
            None => return false,
        };
        let Some(child_key) = self
            .get_container(self.root)
            .and_then(|root| root.child_key(idx))
        else {
            return false;
        };
        let Some(&key) = self.leaf_keys_under(child_key).last() else {
            return false;
        };
        self.focus_node_key(key);
        true
    }

    pub(in crate::layout) fn append_leaf(&mut self, tile: Tile<W>, focus: bool) {
        self.insert_leaf_at(self.root_children_len(), tile, focus);
    }

    pub(in crate::layout) fn insert_leaf_at(&mut self, index: usize, tile: Tile<W>, focus: bool) {
        let root = self.root;
        self.insert_leaf_into_branch(root, index, tile, focus);
    }

    /// Insert a leaf as a direct child of one branch's root.
    pub(in crate::layout) fn insert_leaf_into_branch(
        &mut self,
        branch_root: NodeKey,
        index: usize,
        tile: Tile<W>,
        focus: bool,
    ) {
        let tile_key = self.insert_node(NodeData::Container(ContainerData::new_view(tile)));
        self.insert_key_into_branch(branch_root, index, tile_key, focus);
    }

    pub(super) fn insert_key_at_root(&mut self, index: usize, node_key: NodeKey, focus: bool) {
        let root = self.root;
        self.insert_key_into_branch(root, index, node_key, focus);
    }

    pub(super) fn insert_key_into_branch(
        &mut self,
        branch_root: NodeKey,
        index: usize,
        node_key: NodeKey,
        focus: bool,
    ) {
        let insert_idx = {
            let container_key = branch_root;
            let container = self.get_container(container_key).unwrap();
            let idx = index.min(container.children.len());

            if let Some(container) = self.get_container_mut(container_key) {
                container.insert_child(idx, node_key);

                idx
            } else {
                idx
            }
        };
        let container_key = branch_root;
        if let Some(container) = self.get_container(container_key) {
            if container.child_key(insert_idx) == Some(node_key) {
                self.set_parent(node_key, Some(container_key));
            }
        }

        self.settle_focus_after_insert(node_key, focus);
    }
}
