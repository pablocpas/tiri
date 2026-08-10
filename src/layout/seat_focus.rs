use super::container::InactiveTilingReference;
use super::workspace::WorkspaceId;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SeatFocusNode<WindowId> {
    Workspace {
        workspace_id: WorkspaceId,
        output_name: Option<String>,
    },
    Tiling {
        workspace_id: WorkspaceId,
        reference: InactiveTilingReference,
    },
    Floating {
        workspace_id: WorkspaceId,
        window_id: WindowId,
    },
    Sticky {
        output_name: String,
        window_id: WindowId,
    },
}

impl<WindowId: PartialEq> SeatFocusNode<WindowId> {
    fn same_identity(&self, other: &Self) -> bool {
        match (self, other) {
            (
                Self::Workspace {
                    workspace_id: lhs, ..
                },
                Self::Workspace {
                    workspace_id: rhs, ..
                },
            ) => lhs == rhs,
            (
                Self::Tiling {
                    workspace_id: lhs_ws,
                    reference: lhs_ref,
                },
                Self::Tiling {
                    workspace_id: rhs_ws,
                    reference: rhs_ref,
                },
            ) => lhs_ws == rhs_ws && lhs_ref.node_key() == rhs_ref.node_key(),
            (
                Self::Floating {
                    workspace_id: lhs_ws,
                    window_id: lhs_win,
                },
                Self::Floating {
                    workspace_id: rhs_ws,
                    window_id: rhs_win,
                },
            ) => lhs_ws == rhs_ws && lhs_win == rhs_win,
            (
                Self::Sticky {
                    output_name: lhs_out,
                    window_id: lhs_win,
                },
                Self::Sticky {
                    output_name: rhs_out,
                    window_id: rhs_win,
                },
            ) => lhs_out == rhs_out && lhs_win == rhs_win,
            _ => false,
        }
    }
}

#[derive(Debug, Clone)]
pub struct SeatFocusStack<WindowId> {
    has_layout_focus: bool,
    max_len: usize,
    stack_mru: Vec<SeatFocusNode<WindowId>>,
}

impl<WindowId: Clone + PartialEq> Default for SeatFocusStack<WindowId> {
    fn default() -> Self {
        Self::new()
    }
}

impl<WindowId: Clone + PartialEq> SeatFocusStack<WindowId> {
    pub fn new() -> Self {
        Self {
            has_layout_focus: true,
            max_len: 512,
            stack_mru: Vec::new(),
        }
    }

    pub fn has_layout_focus(&self) -> bool {
        self.has_layout_focus
    }

    pub fn set_has_layout_focus(&mut self, has_layout_focus: bool) {
        self.has_layout_focus = has_layout_focus;
    }

    pub fn set_raw_focus(&mut self, node: SeatFocusNode<WindowId>) {
        if let Some(pos) = self
            .stack_mru
            .iter()
            .position(|existing| existing.same_identity(&node))
        {
            self.stack_mru.remove(pos);
        }
        self.stack_mru.insert(0, node);
        if self.stack_mru.len() > self.max_len {
            self.stack_mru.truncate(self.max_len);
        }
    }

    pub fn set_focus_chain(
        &mut self,
        chain_outer_to_inner: impl IntoIterator<Item = SeatFocusNode<WindowId>>,
    ) {
        for node in chain_outer_to_inner {
            self.set_raw_focus(node);
        }
    }

    #[cfg(test)]
    pub fn len(&self) -> usize {
        self.stack_mru.len()
    }

    #[cfg(test)]
    pub fn max_len(&self) -> usize {
        self.max_len
    }

    pub fn snapshot(&self) -> Vec<SeatFocusNode<WindowId>> {
        self.stack_mru.clone()
    }

    pub fn replace_from_snapshot(&mut self, snapshot: Vec<SeatFocusNode<WindowId>>) {
        self.stack_mru = snapshot;
        if self.stack_mru.len() > self.max_len {
            self.stack_mru.truncate(self.max_len);
        }
    }

    pub fn focus_inactive_workspace(
        &self,
        workspace_id: WorkspaceId,
    ) -> Option<SeatFocusNode<WindowId>> {
        self.stack_mru.iter().find_map(|node| match node {
            SeatFocusNode::Workspace {
                workspace_id: ws_id,
                ..
            }
            | SeatFocusNode::Tiling {
                workspace_id: ws_id,
                ..
            }
            | SeatFocusNode::Floating {
                workspace_id: ws_id,
                ..
            } if *ws_id == workspace_id => Some(node.clone()),
            _ => None,
        })
    }

    pub fn focus_inactive_tiling(
        &self,
        workspace_id: WorkspaceId,
    ) -> Option<InactiveTilingReference> {
        self.stack_mru.iter().find_map(|node| match node {
            SeatFocusNode::Tiling {
                workspace_id: ws_id,
                reference,
            } if *ws_id == workspace_id
                && matches!(reference, InactiveTilingReference::Leaf { .. }) =>
            {
                Some(reference.clone())
            }
            _ => None,
        })
    }

    pub fn focus_inactive_floating(&self, workspace_id: WorkspaceId) -> Option<WindowId> {
        self.stack_mru.iter().find_map(|node| match node {
            SeatFocusNode::Floating {
                workspace_id: ws_id,
                window_id,
            } if *ws_id == workspace_id => Some(window_id.clone()),
            _ => None,
        })
    }

    pub fn focus_inactive_output(&self, output_name: &str) -> Option<SeatFocusNode<WindowId>> {
        self.stack_mru.iter().find_map(|node| match node {
            SeatFocusNode::Workspace {
                output_name: Some(name),
                ..
            } if name == output_name => Some(node.clone()),
            SeatFocusNode::Sticky {
                output_name: name, ..
            } if name == output_name => Some(node.clone()),
            _ => None,
        })
    }
}
