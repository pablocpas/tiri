use std::collections::HashSet;

use super::{ContainerTree, LayoutElement, LeafLayoutInfo, NodeData, NodeKey};

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

        if self.is_empty() {
            // The workspace itself stays: an empty workspace is a container with no
            // children, holding the orientation the next window will be laid out by.
            assert_eq!(
                self.nodes.len(),
                1,
                "empty tree must retain the workspace and nothing else"
            );
            assert!(
                self.focused_key.is_none(),
                "empty tree must not retain focused_key"
            );
            assert!(
                self.selected_key.is_none_or(|key| key == root_key),
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

        assert_eq!(
            visited.len(),
            self.nodes.len(),
            "all nodes must be reachable from root"
        );

        for key in self.nodes.keys() {
            assert!(
                visited.contains(&key),
                "node {key:?} exists in slotmap but is unreachable"
            );
        }

        match self.focused_key {
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

        if let Some(key) = self.selected_key {
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
                assert!(child_count > 0, "container nodes must not be empty");
                assert_eq!(
                    container.child_percents_slice().len(),
                    child_count,
                    "child_percents length must match child count"
                );
                assert_eq!(
                    container.focus_stack.len(),
                    child_count,
                    "focus_stack length must match child count"
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

                let mut focus_set = HashSet::with_capacity(child_count);
                for child in &container.focus_stack {
                    assert!(
                        child_set.contains(child),
                        "focus_stack entries must be children of the container"
                    );
                    assert!(
                        focus_set.insert(*child),
                        "focus_stack must not contain duplicates"
                    );
                }

                let mut percent_sum = 0.0;
                for percent in container.child_percents_slice() {
                    assert!(
                        percent.is_finite() && *percent >= 0.0,
                        "child percents must be finite and non-negative"
                    );
                    percent_sum += *percent;
                }
                assert!(
                    (percent_sum - 1.0).abs() <= 0.000_001,
                    "child percents must be normalized"
                );

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
                self.find_node_path(info.key).as_deref(),
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
