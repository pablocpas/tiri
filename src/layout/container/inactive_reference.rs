//! Workspace-local inactive tiling focus, read directly from the seat order.

use super::{ContainerTree, InsertParentInfo, LayoutElement, NodeData, NodeKey};

impl<W: LayoutElement> ContainerTree<W> {
    /// The most recent node that still belongs to `ws->tiling`.
    ///
    /// Tiling and floating share this arena and this seat order. Floating a subtree removes
    /// it from the tiled branch without disturbing the relative order of the tiled nodes,
    /// which is exactly the state sway reads through `seat_get_focus_inactive_tiling`.
    pub(in crate::layout) fn inactive_tiling_key(&self) -> Option<NodeKey> {
        self.seat.order().iter().copied().find(|key| {
            *key != self.root
                && self.find_node_path(*key).is_some()
                && matches!(
                    self.get_node(*key),
                    Some(NodeData::Leaf(_) | NodeData::Container(_))
                )
        })
    }

    /// Where a floating node rejoins tiling, derived from the same key the seat chose.
    pub(in crate::layout) fn inactive_tiling_restore_target(&self) -> Option<InsertParentInfo> {
        self.insert_parent_info_for_inactive_tiling_key(self.inactive_tiling_key()?)
    }

    fn insert_parent_info_for_inactive_tiling_key(&self, key: NodeKey) -> Option<InsertParentInfo> {
        let path = self.find_node_path(key)?;
        match self.get_node(key)? {
            NodeData::Container(container) => Some(InsertParentInfo {
                parent_path: path,
                insert_idx: container.child_count(),
                layout: container.layout(),
            }),
            NodeData::Leaf(_) => {
                let (leaf_idx, parent_path) = path.split_last()?;
                let parent_key = self.get_node_key_at_path(parent_path)?;
                let parent = self.get_container(parent_key)?;
                Some(InsertParentInfo {
                    parent_path: parent_path.to_vec(),
                    insert_idx: (leaf_idx + 1).min(parent.child_count()),
                    layout: parent.layout(),
                })
            }
        }
    }

    pub(in crate::layout) fn focus_inactive_tiling_key(&mut self, key: NodeKey) -> bool {
        if key == self.root || self.find_node_path(key).is_none() {
            return false;
        }
        self.focus_node_key(key);
        self.layout();
        true
    }

    pub(in crate::layout) fn window_for_inactive_tiling_key(&self, key: NodeKey) -> Option<&W> {
        self.find_node_path(key)?;
        self.get_tile(key).map(|tile| tile.window())
    }
}
