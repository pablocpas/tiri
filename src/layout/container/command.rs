use super::{ContainerTree, LayoutElement, NodeData, NodeKey};

/// Resolved target for commands that may operate on a workspace, container, or leaf.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(in crate::layout) enum TreeCommandTarget {
    Workspace,
    Container(NodeKey),
    Leaf(NodeKey),
}

impl<W: LayoutElement> ContainerTree<W> {
    /// Whether the root of the branch holding `key` is a container a command can be aimed at.
    ///
    /// The workspace is not one: a command that lands there means the workspace, and sway
    /// reaches it through `workspace_*` rather than `container_*`. A floating group's root is
    /// one, because in sway it is an ordinary container that happens to hang off the other
    /// list. This used to be a `RootPolicy` argument threaded through forty-three call sites
    /// and it was never anything but this question, which the tree can now answer for itself.
    pub(in crate::layout) fn branch_is_addressable(&self, key: NodeKey) -> bool {
        self.is_floating(key)
    }

    /// The node a command aimed at `branch_root` operates on.
    ///
    /// The seat is one seat over the whole workspace, so the selection it holds may be in
    /// another branch entirely — the tiled side while a floating group is asking, or the other
    /// way round. A command aimed at a branch resolves inside it or not at all, which is what
    /// restricting both lookups to the branch says.
    pub(in crate::layout) fn command_target_in(&self, branch_root: NodeKey) -> TreeCommandTarget {
        let in_branch = |key: NodeKey| self.is_descendant(key, branch_root);

        if let Some(selected_key) = self.selected_node_key().filter(|key| in_branch(*key)) {
            if matches!(self.get_node(selected_key), Some(NodeData::Container(_))) {
                if selected_key == branch_root && !self.branch_is_addressable(branch_root) {
                    return TreeCommandTarget::Workspace;
                }

                return TreeCommandTarget::Container(selected_key);
            }
        }

        self.focused_node_key()
            .filter(|key| in_branch(*key))
            .or_else(|| self.focus_inactive_view(branch_root))
            .map(TreeCommandTarget::Leaf)
            .unwrap_or(TreeCommandTarget::Workspace)
    }

    /// The branch a resolved target lives in.
    pub(super) fn target_branch_root(&self, target: TreeCommandTarget) -> NodeKey {
        match target {
            TreeCommandTarget::Workspace => self.root,
            TreeCommandTarget::Container(key) | TreeCommandTarget::Leaf(key) => {
                self.branch_root(key)
            }
        }
    }
}
