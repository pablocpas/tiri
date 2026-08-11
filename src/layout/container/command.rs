use super::{ContainerTree, LayoutElement, NodeKey};

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
    pub(in crate::layout) fn command_target_in(&self, branch_root: NodeKey) -> NodeKey {
        let in_branch = |key: NodeKey| self.branch_root(key) == branch_root;

        if let Some(selected_key) = self.selected_key().filter(|key| in_branch(*key)) {
            return selected_key;
        }

        self.focused_node_key()
            .filter(|key| in_branch(*key))
            .or_else(|| self.focus_inactive_view_in_branch(branch_root))
            .unwrap_or(branch_root)
    }
}
