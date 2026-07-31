//! InactiveTilingReference: remembering and resolving restore targets.

use super::ContainerTree;
use super::InactiveTilingReference;
use super::InsertParentInfo;
use super::Layout;
use super::LayoutElement;
use super::NodeData;
use super::NodeKey;
use super::ResolvedInactiveTilingReference;

impl<W: LayoutElement> ContainerTree<W> {
    pub(super) fn inactive_tiling_reference_for_node_key(
        &self,
        key: NodeKey,
    ) -> Option<InactiveTilingReference> {
        let path_hint = self.find_node_path(key)?;
        match self.get_node(key)? {
            NodeData::Container(_) => (!self.is_synthetic_root_container_key(key))
                .then_some(InactiveTilingReference::Container { key, path_hint }),
            NodeData::Leaf(_) => Some(InactiveTilingReference::Leaf { key, path_hint }),
        }
    }

    pub(super) fn inactive_tiling_container_reference_for_key(
        &self,
        key: NodeKey,
    ) -> Option<InactiveTilingReference> {
        self.get_container(key)?;
        if self.is_synthetic_root_container_key(key) {
            return None;
        }
        let path_hint = self.find_node_path(key)?;
        Some(InactiveTilingReference::Container { key, path_hint })
    }

    pub(super) fn resolve_node_key_from_inactive_tiling_reference(
        &self,
        key: NodeKey,
        path_hint: &[usize],
    ) -> Option<NodeKey> {
        if self.get_node(key).is_some() {
            Some(key)
        } else if path_hint.is_empty() {
            self.root
        } else {
            self.get_node_key_at_path(path_hint)
        }
    }

    pub(super) fn resolve_inactive_tiling_reference(
        &self,
        reference: &InactiveTilingReference,
    ) -> Option<ResolvedInactiveTilingReference> {
        match reference {
            InactiveTilingReference::Container { key, path_hint } => {
                let key = self.resolve_node_key_from_inactive_tiling_reference(*key, path_hint)?;
                self.get_container(key)?;
                if self.is_synthetic_root_container_key(key) {
                    return None;
                }
                let path = self.find_node_path(key)?;
                Some(ResolvedInactiveTilingReference::Container { key, path })
            }
            InactiveTilingReference::Leaf { key, path_hint } => {
                let key = self.resolve_node_key_from_inactive_tiling_reference(*key, path_hint)?;
                if !matches!(self.get_node(key), Some(NodeData::Leaf(_))) {
                    return None;
                }
                let path = self.find_node_path(key)?;
                Some(ResolvedInactiveTilingReference::Leaf { path })
            }
        }
    }

    pub(super) fn resolve_inactive_tiling_reference_strict_key(
        &self,
        reference: &InactiveTilingReference,
    ) -> Option<ResolvedInactiveTilingReference> {
        match reference {
            InactiveTilingReference::Container { key, .. } => {
                self.get_container(*key)?;
                if self.is_synthetic_root_container_key(*key) {
                    return None;
                }
                let path = self.find_node_path(*key)?;
                Some(ResolvedInactiveTilingReference::Container { key: *key, path })
            }
            InactiveTilingReference::Leaf { key, .. } => {
                if !matches!(self.get_node(*key), Some(NodeData::Leaf(_))) {
                    return None;
                }
                let path = self.find_node_path(*key)?;
                Some(ResolvedInactiveTilingReference::Leaf { path })
            }
        }
    }

    pub(in crate::layout) fn inactive_tiling_reference_chain_for_focused_reference(
        &self,
    ) -> Vec<InactiveTilingReference> {
        let mut chain = Vec::new();
        // When a container is selected by focus-parent, that selected node is
        // the active reference source.
        let Some(mut key) = self
            .selected_node_key()
            .or(self.focused_key)
            .or_else(|| self.first_leaf_key())
        else {
            return chain;
        };

        if let Some(reference) = self.inactive_tiling_reference_for_node_key(key) {
            chain.push(reference);
        }

        while let Some(parent_key) = self.parent_of(key) {
            if let Some(reference) = self.inactive_tiling_container_reference_for_key(parent_key) {
                chain.push(reference);
            }
            key = parent_key;
        }

        chain
    }

    pub(in crate::layout) fn inactive_tiling_reference_for_selected_or_focused(
        &self,
    ) -> Option<InactiveTilingReference> {
        let key = self
            .selected_node_key()
            .or(self.focused_key)
            .or_else(|| self.first_leaf_key())?;
        self.inactive_tiling_reference_for_node_key(key)
    }

    pub(in crate::layout) fn inactive_tiling_reference_chain_for_focused_leaf(
        &self,
    ) -> Vec<InactiveTilingReference> {
        let mut chain = Vec::new();
        let Some(mut key) = self.focused_key.or_else(|| self.first_leaf_key()) else {
            return chain;
        };

        if let Some(reference) = self.inactive_tiling_reference_for_node_key(key) {
            chain.push(reference);
        }

        while let Some(parent_key) = self.parent_of(key) {
            if let Some(reference) = self.inactive_tiling_container_reference_for_key(parent_key) {
                chain.push(reference);
            }
            key = parent_key;
        }

        chain
    }

    pub(in crate::layout) fn inactive_tiling_reference_for_parent_of_selected_reference(
        &self,
    ) -> Option<InactiveTilingReference> {
        let key = self.selected_node_key()?;
        let parent_key = self.parent_of(key)?;
        self.inactive_tiling_container_reference_for_key(parent_key)
    }

    pub(in crate::layout) fn inactive_tiling_reference_for_parent_of_window(
        &self,
        window_id: &W::Id,
    ) -> Option<InactiveTilingReference> {
        let path = self.find_window(window_id)?;
        let node_key = self.get_node_key_at_path(&path)?;
        let parent_key = self.parent_of(node_key)?;
        self.inactive_tiling_container_reference_for_key(parent_key)
    }

    /// Resolve an inactive tiling reference with sway semantics:
    /// - container reference => insert as child of that container
    /// - leaf reference => insert as sibling after that leaf
    pub(super) fn insert_parent_info_from_resolved_inactive_tiling_reference(
        &self,
        resolved: ResolvedInactiveTilingReference,
    ) -> Option<InsertParentInfo> {
        match resolved {
            ResolvedInactiveTilingReference::Container {
                key: container_key,
                path,
            } => {
                let container = self.get_container(container_key)?;
                Some(InsertParentInfo {
                    parent_path: path,
                    insert_idx: container.child_count(),
                    layout: container.layout(),
                    child_percents: Vec::new(),
                })
            }
            ResolvedInactiveTilingReference::Leaf { path, .. } => {
                if path.is_empty() {
                    return Some(InsertParentInfo {
                        parent_path: Vec::new(),
                        insert_idx: 1,
                        layout: self.pending_layout.unwrap_or(Layout::SplitH),
                        child_percents: Vec::new(),
                    });
                }

                let parent_path = path[..path.len() - 1].to_vec();
                let parent_key = if parent_path.is_empty() {
                    self.root?
                } else {
                    self.get_node_key_at_path(&parent_path)?
                };
                let parent = self.get_container(parent_key)?;
                let leaf_idx = *path.last().unwrap();
                Some(InsertParentInfo {
                    parent_path,
                    insert_idx: (leaf_idx + 1).min(parent.child_count()),
                    layout: parent.layout(),
                    child_percents: Vec::new(),
                })
            }
        }
    }

    pub(in crate::layout) fn insert_parent_info_from_inactive_tiling_reference(
        &self,
        reference: &InactiveTilingReference,
    ) -> Option<InsertParentInfo> {
        let resolved = self.resolve_inactive_tiling_reference(reference)?;
        self.insert_parent_info_from_resolved_inactive_tiling_reference(resolved)
    }

    pub(in crate::layout) fn insert_parent_info_from_inactive_tiling_reference_strict(
        &self,
        reference: &InactiveTilingReference,
    ) -> Option<InsertParentInfo> {
        let resolved = self.resolve_inactive_tiling_reference_strict_key(reference)?;
        self.insert_parent_info_from_resolved_inactive_tiling_reference(resolved)
    }

    pub(in crate::layout) fn inactive_tiling_reference_is_root_container_strict(
        &self,
        reference: &InactiveTilingReference,
    ) -> bool {
        matches!(
            self.resolve_inactive_tiling_reference_strict_key(reference),
            Some(ResolvedInactiveTilingReference::Container { path, .. }) if path.is_empty()
        )
    }

    pub(in crate::layout) fn has_inactive_tiling_reference(
        &self,
        reference: &InactiveTilingReference,
        strict: bool,
    ) -> bool {
        if strict {
            self.resolve_inactive_tiling_reference_strict_key(reference)
                .is_some()
        } else {
            self.resolve_inactive_tiling_reference(reference).is_some()
        }
    }

    pub(in crate::layout) fn focus_inactive_tiling_reference(
        &mut self,
        reference: &InactiveTilingReference,
        strict: bool,
    ) -> bool {
        let resolved = if strict {
            self.resolve_inactive_tiling_reference_strict_key(reference)
        } else {
            self.resolve_inactive_tiling_reference(reference)
        };
        let Some(resolved) = resolved else {
            return false;
        };

        let key = match resolved {
            ResolvedInactiveTilingReference::Container { key, .. } => key,
            ResolvedInactiveTilingReference::Leaf { path } => {
                let Some(key) = self.get_node_key_at_path(&path) else {
                    return false;
                };
                key
            }
        };

        self.focus_node_key(key);
        self.layout();
        true
    }

    pub(in crate::layout) fn window_for_inactive_tiling_reference(
        &self,
        reference: &InactiveTilingReference,
        strict: bool,
    ) -> Option<&W> {
        let resolved = if strict {
            self.resolve_inactive_tiling_reference_strict_key(reference)
        } else {
            self.resolve_inactive_tiling_reference(reference)
        }?;

        let path = match resolved {
            ResolvedInactiveTilingReference::Leaf { path } => path,
            ResolvedInactiveTilingReference::Container { .. } => return None,
        };

        let key = self.get_node_key_at_path(&path)?;
        match self.get_node(key)? {
            NodeData::Leaf(tile) => Some(tile.window()),
            NodeData::Container(_) => None,
        }
    }
}
