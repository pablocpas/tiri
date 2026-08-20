use std::collections::HashSet;

use super::{ContainerArena, Layout, LayoutElement, LeafLayoutInfo, NodeData, NodeKey};

impl<W: LayoutElement> ContainerArena<W> {
    pub(in crate::layout) fn verify_invariants(&self) {
        let root_key = self.root;

        assert!(
            self.nodes.contains_key(root_key),
            "root key must point to an existing node"
        );
        assert!(
            matches!(self.get_node(root_key), Some(NodeData::Workspace(_))),
            "workspace root must be a workspace node"
        );
        assert_eq!(
            self.parents.get(root_key).copied().flatten(),
            None,
            "root parent must be None"
        );
        if let Some(fullscreen_key) = self.fullscreen_key {
            assert_ne!(
                fullscreen_key, root_key,
                "the workspace node cannot own fullscreen"
            );
            assert!(
                self.holds_node(fullscreen_key),
                "workspace fullscreen must point to one live node"
            );
        }
        self.verify_floating_region();
        self.verify_seat_order();

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
        // The floating groups have the workspace as semantic parent, but live in its separate
        // floating list rather than its tiled child list.
        for floating_root in self.floating_roots().collect::<Vec<_>>() {
            self.verify_node(floating_root, Some(root_key), &mut visited, &mut leaves);
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

        if let Some(fullscreen_key) = self.fullscreen_key {
            assert!(
                visited.contains(&fullscreen_key),
                "workspace fullscreen node must be reachable"
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
        for pending in &self.pending_layouts {
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

        let sizing = match self.get_node(key).expect("visited node must exist") {
            NodeData::Workspace(_) => None,
            NodeData::Container(container) => Some(container.sizing()),
        };
        if let Some(sizing) = sizing {
            for (axis, percent) in [
                ("horizontal", sizing.fractions.width),
                ("vertical", sizing.fractions.height),
            ] {
                assert!(
                    percent.is_finite() && percent >= 0.0,
                    "{axis} fraction must be finite and non-negative"
                );
            }
            for (axis, total) in [
                ("horizontal", sizing.child_total_width),
                ("vertical", sizing.child_total_height),
            ] {
                assert!(
                    total.is_finite() && total >= 0.0,
                    "{axis} child total must be finite and non-negative"
                );
            }
        }

        match self.get_node(key).expect("visited node must exist") {
            node if node.is_view() => {
                leaves.insert(key);
            }
            NodeData::Workspace(_) | NodeData::Container(_) => {
                let container = self
                    .get_container(key)
                    .expect("workspace and container nodes are layout parents");
                let child_count = container.child_count();
                assert!(
                    child_count > 0 || key == self.root,
                    "container nodes other than the workspace must not be empty"
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

                if child_count > 0 && matches!(container.layout, Layout::SplitH | Layout::SplitV) {
                    let percents = self.child_percents(key);
                    let percent_sum: f64 = percents.iter().sum();
                    assert!(
                        (percent_sum - 1.0).abs() <= 0.000_001,
                        "active split child percents must be normalized: layout={:?} fractions={:?}",
                        container.layout,
                        percents,
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

impl<W: LayoutElement> ContainerArena<W> {
    /// The seat's order is the workspace's one focus cache, and everything reads through it.
    ///
    /// It is sway's `seat->focus_stack`, which its new-node and destroy listeners keep as
    /// exactly the live nodes. Nothing derived from it can be checked in its own right —
    /// `inactive_tiling_key` and `seat_get_active_tiling_child` are filters over this list, so
    /// asserting their answers only restates the filter. This is the state they read.
    ///
    /// A missing node is a node no descent can land on and no switcher will show; a stale one
    /// is a key that outranks every live node and answers for a window that is gone.
    fn verify_seat_order(&self) {
        let order = self.seat.order();
        let mut seen = HashSet::new();
        for key in order {
            assert!(
                self.nodes.contains_key(*key),
                "the seat's order may only name live nodes, found {key:?}"
            );
            assert!(
                seen.insert(*key),
                "a node must appear once in the seat's order: with the same one twice, \
                 \"most recently focused\" does not name anything"
            );
        }
        assert_eq!(
            seen.len(),
            self.nodes.len(),
            "every live node has a place in the seat's order, however far back: a node that is \
             not in it is one `focus_inactive` can never answer with"
        );

        if let Some(leaf) = self.seat.focused_leaf() {
            assert!(
                self.get_node(leaf).is_some_and(|node| node.is_view()),
                "the seat's keyboard focus is a window, found {leaf:?}"
            );
        }
        if let Some(selected) = self.seat.selected() {
            assert!(
                self.nodes.contains_key(selected),
                "the seat's selection must be a live node, found {selected:?}"
            );
        }
    }

    /// The floating side is in the same arena, and has to look like it.
    ///
    /// Every floating root is a live node parented semantically by the workspace, listed once.
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
                Some(self.root),
                "a floating root must have the workspace as semantic parent"
            );
            assert_ne!(
                key, self.root,
                "the workspace cannot be one of its own floating groups"
            );
            assert!(
                seen.insert(key),
                "a node must not be listed as a floating root twice"
            );

            // A view is one too: sway's `ws->floating` holds whatever was floated, so the
            // floating root is a split only when the user built one (sway/tree/container.c:1104).
            let container = self
                .get_any_container(key)
                .expect("every floating root must be a container");
            let geometry = container
                .floating_geometry
                .expect("every floating root must own geometry state");
            let target = geometry.target;
            assert!(
                target.loc.x.is_finite()
                    && target.loc.y.is_finite()
                    && target.size.w.is_finite()
                    && target.size.h.is_finite()
                    && target.size.w >= 0.0
                    && target.size.h >= 0.0,
                "a floating target geometry must be finite and non-negative"
            );
            let resize_base = geometry.resize_base_size;
            assert!(
                resize_base.w.is_finite()
                    && resize_base.h.is_finite()
                    && resize_base.w >= 0.0
                    && resize_base.h >= 0.0,
                "a floating resize base must be finite and non-negative"
            );
            assert!(
                geometry.pos.x.is_finite()
                    && geometry.pos.y.is_finite()
                    && geometry.working_area.loc.x.is_finite()
                    && geometry.working_area.loc.y.is_finite()
                    && geometry.working_area.size.w.is_finite()
                    && geometry.working_area.size.h.is_finite()
                    && geometry.working_area.size.w >= 0.0
                    && geometry.working_area.size.h >= 0.0,
                "floating relative geometry must be finite with a non-negative working area"
            );
        }
    }
}
