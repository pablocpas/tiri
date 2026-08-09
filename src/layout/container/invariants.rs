use std::collections::HashSet;

use super::{ContainerTree, Layout, LayoutElement, LeafLayoutInfo, NodeData, NodeKey};

impl<W: LayoutElement> ContainerTree<W> {
    pub(in crate::layout) fn verify_invariants(&self) {
        let root_key = self.root;

        assert!(
            self.nodes.contains_key(root_key),
            "root key must point to an existing node"
        );
        assert!(
            matches!(self.get_node(root_key), Some(NodeData::Container(_))),
            "workspace root must be a container"
        );
        assert_eq!(
            self.parents.get(root_key).copied().flatten(),
            None,
            "root parent must be None"
        );

        self.verify_floating_region();

        if self.is_empty() && self.floating_roots().next().is_none() {
            // The workspace itself stays: an empty workspace is a container with no
            // children, holding the orientation the next window will be laid out by.
            assert_eq!(
                self.nodes.len(),
                1,
                "empty tree must retain the workspace and nothing else"
            );
            assert!(
                self.focused_key().is_none(),
                "empty tree must not retain focused_key"
            );
            assert!(
                self.selected_key().is_none_or(|key| key == root_key),
                "empty tree must not retain a selected_key below the workspace"
            );
            assert!(
                self.leaf_layouts.is_empty(),
                "empty tree must not retain leaf layout cache"
            );
            return;
        }

        let mut visited = HashSet::new();
        let mut leaves = HashSet::new();
        self.verify_node(root_key, None, &mut visited, &mut leaves);
        // The floating groups are roots of their own, the way sway's `ws->floating` holds
        // containers that hang off the workspace's list rather than off its tiling tree.
        // Reachability is from any root, not from the tiled one.
        for floating_root in self.floating_roots().collect::<Vec<_>>() {
            self.verify_node(floating_root, None, &mut visited, &mut leaves);
        }

        assert_eq!(
            visited.len(),
            self.nodes.len(),
            "every node must be reachable from the workspace root or from a floating root"
        );

        for key in self.nodes.keys() {
            assert!(
                visited.contains(&key),
                "node {key:?} exists in the workspace store but is unreachable"
            );
        }

        match self.focused_key() {
            Some(key) => {
                assert!(
                    leaves.contains(&key),
                    "focused_key must point to a leaf in the tree"
                );
            }
            None => {
                assert!(
                    leaves.is_empty(),
                    "non-empty tree with leaves must have a focused leaf"
                );
            }
        }

        if let Some(key) = self.selected_key() {
            assert!(
                visited.contains(&key),
                "selected_key must point to a node in the tree"
            );
        }

        self.verify_leaf_layout_cache(self.leaf_layouts.as_slice(), &leaves, "leaf_layouts");
        if let Some(pending) = &self.pending_layouts {
            self.verify_leaf_layout_cache(
                pending.data.leaf_layouts.as_slice(),
                &leaves,
                "pending leaf_layouts",
            );
        }

        assert!(
            self.get_container(root_key).is_some(),
            "the workspace must be a container"
        );
    }

    fn verify_node(
        &self,
        key: NodeKey,
        expected_parent: Option<NodeKey>,
        visited: &mut HashSet<NodeKey>,
        leaves: &mut HashSet<NodeKey>,
    ) {
        assert!(
            visited.insert(key),
            "container tree must not contain cycles"
        );
        assert_eq!(
            self.parents.get(key).copied().flatten(),
            expected_parent,
            "node parent pointer must match parent child list"
        );

        match self.get_node(key).expect("visited node must exist") {
            NodeData::Leaf(_) => {
                leaves.insert(key);
            }
            NodeData::Container(container) => {
                let child_count = container.child_count();
                assert!(
                    child_count > 0 || key == self.root,
                    "container nodes other than the workspace must not be empty"
                );
                assert_eq!(
                    container.fractions.horizontal.len(),
                    child_count,
                    "horizontal fractions length must match child count"
                );
                assert_eq!(
                    container.fractions.vertical.len(),
                    child_count,
                    "vertical fractions length must match child count"
                );

                let mut child_set = HashSet::with_capacity(child_count);
                for child in container.children() {
                    assert!(
                        self.nodes.contains_key(*child),
                        "container child must point to an existing node"
                    );
                    assert!(
                        child_set.insert(*child),
                        "container children must not contain duplicates"
                    );
                }

                for (axis, fractions) in [
                    ("horizontal", &container.fractions.horizontal),
                    ("vertical", &container.fractions.vertical),
                ] {
                    for percent in fractions {
                        assert!(
                            percent.is_finite() && *percent >= 0.0,
                            "{axis} fractions must be finite and non-negative"
                        );
                    }
                }
                if child_count > 0 && matches!(container.layout, Layout::SplitH | Layout::SplitV) {
                    let percent_sum: f64 = container.child_percents_slice().iter().sum();
                    assert!(
                        (percent_sum - 1.0).abs() <= 0.000_001,
                        "active split child percents must be normalized: layout={:?} fractions={:?}",
                        container.layout,
                        container.child_percents_slice(),
                    );
                }

                for child in container.children() {
                    self.verify_node(*child, Some(key), visited, leaves);
                }
            }
        }
    }

    fn verify_leaf_layout_cache(
        &self,
        layouts: &[LeafLayoutInfo],
        leaves: &HashSet<NodeKey>,
        label: &str,
    ) {
        let mut seen = HashSet::with_capacity(layouts.len());
        for info in layouts {
            assert!(
                leaves.contains(&info.key),
                "{label} entry must point to a leaf"
            );
            assert!(
                seen.insert(info.key),
                "{label} must not contain duplicate leaf keys"
            );
            assert_eq!(
                self.branch_relative_path(info.key).as_deref(),
                Some(info.path.as_slice()),
                "{label} path must match the current tree"
            );
            assert!(
                info.rect.size.w >= 0.0 && info.rect.size.h >= 0.0,
                "{label} rectangles must not have negative size"
            );
        }
    }
}

impl<W: LayoutElement> ContainerTree<W> {
    /// The floating side is in the same arena, and has to look like it.
    ///
    /// Every floating root is a live node with no parent, listed once. The whole point of
    /// holding both sides here is that a node keeps its key when it crosses, so a stale entry
    /// would be worse than the two-tree model it replaces: a key that still resolves, still
    /// answers, and belongs to a branch nobody can reach.
    fn verify_floating_region(&self) {
        let mut seen = HashSet::new();
        for key in self.floating_roots() {
            assert!(
                self.nodes.contains_key(key),
                "a floating root must point to an existing node"
            );
            assert_eq!(
                self.parents.get(key).copied().flatten(),
                None,
                "a floating root must have no parent — that is what makes it a root"
            );
            assert_ne!(
                key, self.root,
                "the workspace cannot be one of its own floating groups"
            );
            assert!(
                seen.insert(key),
                "a node must not be listed as a floating root twice"
            );
        }
    }
}
