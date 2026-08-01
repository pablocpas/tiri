//! Slotmap-backed node storage primitives: raw node access and parent links.

use super::ContainerData;
use super::ContainerTree;
use super::LayoutElement;
use super::NodeData;
use super::NodeKey;
use crate::layout::tile::Tile;

impl<W: LayoutElement> ContainerTree<W> {
    /// The implicit workspace-root container is an implementation detail and
    /// should be ignored in inactive-tiling reference resolution.
    pub(super) fn is_synthetic_root_container_key(&self, key: NodeKey) -> bool {
        if self.root != Some(key) {
            return false;
        }

        let Some(container) = self.get_container(key) else {
            return false;
        };

        // Explicit root wrappers created by root-level layout commands are real
        // restore/focus targets. Only the implicit workspace backing root should
        // be treated as synthetic.
        !container.preserve_on_single()
    }

    /// Get node data by key
    pub(super) fn get_node(&self, key: NodeKey) -> Option<&NodeData<W>> {
        self.nodes.get(key)
    }

    /// Get mutable node data by key
    pub(super) fn get_node_mut(&mut self, key: NodeKey) -> Option<&mut NodeData<W>> {
        self.nodes.get_mut(key)
    }

    /// Get container data by key
    pub(super) fn get_container(&self, key: NodeKey) -> Option<&ContainerData> {
        match self.nodes.get(key)? {
            NodeData::Container(container) => Some(container),
            _ => None,
        }
    }

    /// Get mutable container data by key
    pub(super) fn get_container_mut(&mut self, key: NodeKey) -> Option<&mut ContainerData> {
        match self.nodes.get_mut(key)? {
            NodeData::Container(container) => Some(container),
            _ => None,
        }
    }

    pub(super) fn set_parent(&mut self, child: NodeKey, parent: Option<NodeKey>) {
        if let Some(entry) = self.parents.get_mut(child) {
            *entry = parent;
        } else {
            self.parents.insert(child, parent);
        }
    }

    pub(super) fn parent_of(&self, key: NodeKey) -> Option<NodeKey> {
        self.parents.get(key).and_then(|parent| *parent)
    }

    pub(super) fn child_index(&self, parent_key: NodeKey, child_key: NodeKey) -> Option<usize> {
        self.get_container(parent_key)?
            .children
            .iter()
            .position(|&key| key == child_key)
    }

    /// Get tile by key (O(1) access).
    pub(in crate::layout) fn get_tile(&self, key: NodeKey) -> Option<&Tile<W>> {
        match self.nodes.get(key)? {
            NodeData::Leaf(tile) => Some(tile),
            _ => None,
        }
    }

    /// Get mutable tile by key (O(1) access).
    pub(in crate::layout) fn get_tile_mut(&mut self, key: NodeKey) -> Option<&mut Tile<W>> {
        match self.nodes.get_mut(key)? {
            NodeData::Leaf(tile) => Some(tile),
            _ => None,
        }
    }

    /// Insert a new node into the slotmap
    pub(super) fn insert_node(&mut self, node: NodeData<W>) -> NodeKey {
        let key = self.nodes.insert(node);
        self.parents.insert(key, None);
        key
    }

    /// Remove a node from the slotmap (and recursively all its children)
    pub(super) fn remove_node_recursive(&mut self, key: NodeKey) -> Option<NodeData<W>> {
        let node = self.nodes.remove(key)?;
        self.parents.remove(key);

        // If it's a container, recursively remove all children
        if let NodeData::Container(ref container) = node {
            for &child_key in &container.children {
                self.remove_node_recursive(child_key);
            }
        }

        Some(node)
    }

    /// Check if tree is empty
    pub(in crate::layout) fn is_empty(&self) -> bool {
        self.root.is_none()
    }

    /// The root key, asserting the tree is non-empty.
    ///
    /// Callers must have already established that the tree has a root (typically via an
    /// `is_none()` early return). Use this instead of `self.root.unwrap()` so the precondition
    /// is named at the panic site.
    pub(super) fn expect_root(&self) -> NodeKey {
        self.root.expect("container tree root must exist here")
    }

    /// Take the root key out, asserting the tree is non-empty. See [`Self::expect_root`].
    pub(super) fn take_root(&mut self) -> NodeKey {
        self.root
            .take()
            .expect("container tree root must exist here")
    }

    pub(in crate::layout) fn root_is_synthetic_workspace_container(&self) -> bool {
        self.root
            .is_some_and(|root_key| self.is_synthetic_root_container_key(root_key))
    }

    /// Parent of a node, or None for the root.
    pub(in crate::layout) fn parent_of_node(&self, key: NodeKey) -> Option<NodeKey> {
        self.parent_of(key)
    }

    /// Whether `key` is `ancestor` or sits somewhere below it.
    pub(in crate::layout) fn is_descendant(&self, key: NodeKey, ancestor: NodeKey) -> bool {
        self.is_descendant_of(key, ancestor)
    }

    /// Whether `key` is `ancestor` or sits somewhere below it.
    pub(super) fn is_descendant_of(&self, key: NodeKey, ancestor: NodeKey) -> bool {
        let mut current = key;
        loop {
            if current == ancestor {
                return true;
            }
            match self.parent_of(current) {
                Some(parent) => current = parent,
                None => return false,
            }
        }
    }
}
