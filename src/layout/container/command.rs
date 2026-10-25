use super::{ContainerTree, LayoutElement, LayoutMode, NodeKey};
use crate::layout::LayoutCycleEntry;

/// A `split` or `layout` command, as sway's `cmd_split` and `cmd_layout` take them.
///
/// What it acts on is resolved by the tree from the seat, the same way for the tiled side and
/// for a floating group; see `ContainerTree::apply_layout_command`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LayoutCommand<'a> {
    /// `split horizontal|vertical`.
    Split(LayoutMode),
    /// `split toggle`: split the opposite way to the parent.
    SplitToggle,
    /// `split none`.
    Unsplit,
    /// `layout splith|splitv|tabbed|stacking`.
    Set(LayoutMode),
    /// `layout default`.
    Default,
    /// `layout toggle split`, or `layout toggle` with nothing after it.
    ToggleSplit,
    /// `layout toggle all`.
    ToggleAll,
    /// `layout toggle <layout>...`.
    ToggleCycle(&'a [LayoutCycleEntry]),
}

/// Semantic target of a command resolved for one workspace.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(in crate::layout) enum WorkspaceCommandTarget {
    Workspace,
    Tiled(NodeKey),
    Floating(NodeKey),
}

impl WorkspaceCommandTarget {
    pub(in crate::layout) fn key(self, workspace: NodeKey) -> NodeKey {
        match self {
            Self::Workspace => workspace,
            Self::Tiled(key) | Self::Floating(key) => key,
        }
    }
}

impl<W: LayoutElement> ContainerTree<W> {
    /// Resolve a workspace command once, including which root collection owns its target.
    pub(in crate::layout) fn workspace_command_target(
        &self,
        workspace: NodeKey,
    ) -> WorkspaceCommandTarget {
        let Some(key) = self.selected_node_key(workspace) else {
            return WorkspaceCommandTarget::Workspace;
        };
        if key == workspace {
            WorkspaceCommandTarget::Workspace
        } else if self.is_in_floating_branch(key) {
            WorkspaceCommandTarget::Floating(key)
        } else {
            WorkspaceCommandTarget::Tiled(key)
        }
    }

    /// The selected command node, or `None` when the workspace itself is the target.
    pub(in crate::layout) fn workspace_command_node(&self, workspace: NodeKey) -> Option<NodeKey> {
        match self.workspace_command_target(workspace) {
            WorkspaceCommandTarget::Workspace => None,
            WorkspaceCommandTarget::Tiled(key) | WorkspaceCommandTarget::Floating(key) => Some(key),
        }
    }

    /// Whether the root of the branch holding `key` is a container a command can be aimed at.
    ///
    /// The workspace is not one: a command that lands there means the workspace, and sway
    /// reaches it through `workspace_*` rather than `container_*`. A floating group's root is
    /// one, because in sway it is an ordinary container that happens to hang off the other
    /// list.
    pub(in crate::layout) fn branch_is_addressable(&self, key: NodeKey) -> bool {
        self.is_floating_root(key)
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
