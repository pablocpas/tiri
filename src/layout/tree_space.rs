//! Helpers shared between the tiling space and the floating containers — the two
//! consumers of a `ContainerTree`.

use super::container::{ContainerTree, LeafLayoutInfo};
use super::LayoutElement;

/// Leaf layouts to display: the committed layouts, falling back to pending ones while a
/// resize transaction is still in flight.
pub(super) fn display_layouts<W: LayoutElement>(tree: &ContainerTree<W>) -> &[LeafLayoutInfo] {
    if tree.leaf_layouts().is_empty() {
        tree.pending_leaf_layouts()
            .unwrap_or_else(|| tree.leaf_layouts())
    } else {
        tree.leaf_layouts()
    }
}

/// Span available to a container's children after subtracting inter-child gaps.
pub(super) fn available_span(gap: f64, total: f64, child_count: usize) -> f64 {
    if child_count == 0 {
        return 0.0;
    }
    (total - gap * (child_count as f64 - 1.0)).max(0.0)
}

/// A tree mutation's report of whether it changed anything.
///
/// Implemented for `bool` (the op tells us) and for `()` (the op has no failure signal, so
/// assume it changed). Used by the spaces' `mutate_tree` so a mutation can never be
/// committed without the matching relayout.
pub(super) trait TreeMutation {
    fn changed(&self) -> bool;
}

impl TreeMutation for bool {
    fn changed(&self) -> bool {
        *self
    }
}

impl TreeMutation for () {
    fn changed(&self) -> bool {
        true
    }
}

/// A mutation that yields something only when it did something, such as taking a subtree
/// out of the tree.
impl<T> TreeMutation for Option<T> {
    fn changed(&self) -> bool {
        self.is_some()
    }
}
