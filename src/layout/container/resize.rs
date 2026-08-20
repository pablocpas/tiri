//! Interactive resize targeting: which pair of siblings a drag on a window edge resizes.

use smithay::utils::{Logical, Point};

use super::ContainerArena;
use super::Layout;
use super::LayoutElement;
use super::NodeKey;
use crate::utils::ResizeEdge;

/// A pair of adjacent children inside a split container: dragging the boundary between
/// them grows one at the other's expense.
///
/// Keyed by `NodeKey` rather than by tree path so it survives structural mutations during
/// the drag and needs no re-resolution on every pointer motion.
#[derive(Debug, Clone, Copy, PartialEq)]
pub(in crate::layout) struct ResizeTarget {
    /// Split container holding both children.
    pub parent: NodeKey,
    /// The child being resized.
    pub child: NodeKey,
    /// The sibling absorbing the change.
    pub neighbor: NodeKey,
    /// Whether the neighbor sits after the child along the container's axis.
    pub neighbor_after: bool,
    /// The child's span along the axis when the resize began, in logical pixels.
    pub original_span: f64,
}

fn is_horizontal(edge: ResizeEdge) -> bool {
    edge == ResizeEdge::LEFT || edge == ResizeEdge::RIGHT
}

fn is_vertical(edge: ResizeEdge) -> bool {
    edge == ResizeEdge::TOP || edge == ResizeEdge::BOTTOM
}

impl<W: LayoutElement> ContainerArena<W> {
    /// Resolve the resize targets for a drag on `window`'s `edges`.
    ///
    /// Returns the edges that actually resolved to a target (a window with no resizable
    /// neighbour on an axis drops that axis) plus the horizontal and vertical targets.
    /// `pos` is the pointer position, used to disambiguate between nested split ancestors
    /// by picking the boundary closest to the cursor; without it the innermost ancestor
    /// wins.
    pub(in crate::layout) fn resize_targets_for_window(
        &self,
        window: &W::Id,
        mut edges: ResizeEdge,
        pos: Option<Point<f64, Logical>>,
    ) -> Option<(ResizeEdge, Option<ResizeTarget>, Option<ResizeTarget>)> {
        let leaf_key = self.window_key(window)?;
        let tile = self.get_tile(leaf_key)?;

        if !tile.window().pending_sizing_mode().is_normal() {
            return None;
        }

        let mut horizontal = None;
        let mut vertical = None;

        if edges.intersects(ResizeEdge::LEFT_RIGHT) {
            let edge = if edges.contains(ResizeEdge::LEFT) {
                ResizeEdge::LEFT
            } else {
                ResizeEdge::RIGHT
            };
            horizontal = self.resize_target_for_edge(leaf_key, edge, Layout::SplitH, pos);
            if horizontal.is_none() {
                edges.remove(ResizeEdge::LEFT_RIGHT);
            }
        }

        if edges.intersects(ResizeEdge::TOP_BOTTOM) {
            let edge = if edges.contains(ResizeEdge::TOP) {
                ResizeEdge::TOP
            } else {
                ResizeEdge::BOTTOM
            };
            vertical = self.resize_target_for_edge(leaf_key, edge, Layout::SplitV, pos);
            if vertical.is_none() {
                edges.remove(ResizeEdge::TOP_BOTTOM);
            }
        }

        if edges.is_empty() {
            return None;
        }

        Some((edges, horizontal, vertical))
    }

    /// Whether a drag on `edge` of the leaf at `leaf_key` would resize anything.
    pub(in crate::layout) fn has_resize_target(
        &self,
        leaf_key: NodeKey,
        edge: ResizeEdge,
        layout: Layout,
        pos: Point<f64, Logical>,
    ) -> bool {
        self.resize_target_for_edge(leaf_key, edge, layout, Some(pos))
            .is_some()
    }

    /// Walk from `leaf_key` towards the root looking for a split container along `layout`'s
    /// axis where the node has a neighbour on `edge`'s side.
    fn resize_target_for_edge(
        &self,
        leaf_key: NodeKey,
        edge: ResizeEdge,
        layout: Layout,
        pos: Option<Point<f64, Logical>>,
    ) -> Option<ResizeTarget> {
        let mut best: Option<(ResizeTarget, f64)> = None;
        let mut innermost = None;
        let mut current = leaf_key;

        while let Some(parent_key) = self.parent_of(current) {
            let Some(container) = self.get_container(parent_key) else {
                current = parent_key;
                continue;
            };
            let child_count = container.child_count();

            if container.layout() == layout && child_count > 1 {
                if let Some(target) = self.resize_target_in(parent_key, current, edge) {
                    innermost.get_or_insert(target);

                    if let Some(pos) = pos {
                        if let Some(boundary) = self.resize_boundary_coord(&target, edge) {
                            let dist = if is_horizontal(edge) {
                                (pos.x - boundary).abs()
                            } else if is_vertical(edge) {
                                (pos.y - boundary).abs()
                            } else {
                                f64::MAX
                            };

                            let closer = best
                                .as_ref()
                                .is_none_or(|(_, best_dist)| dist + f64::EPSILON < *best_dist);
                            if closer {
                                best = Some((target, dist));
                            }
                        }
                    }
                }
            }

            current = parent_key;
        }

        best.map(|(target, _)| target).or(innermost)
    }

    /// Build the target for resizing `child` against its neighbour on `edge`'s side inside
    /// `parent`, if such a neighbour exists.
    fn resize_target_in(
        &self,
        parent: NodeKey,
        child: NodeKey,
        edge: ResizeEdge,
    ) -> Option<ResizeTarget> {
        let container = self.get_container(parent)?;
        let child_count = container.child_count();
        let child_idx = self.child_index(parent, child)?;

        let neighbor_idx = if edge == ResizeEdge::LEFT || edge == ResizeEdge::TOP {
            child_idx.checked_sub(1)
        } else if edge == ResizeEdge::RIGHT || edge == ResizeEdge::BOTTOM {
            (child_idx + 1 < child_count).then_some(child_idx + 1)
        } else {
            None
        }?;

        let neighbor = self.get_container(parent)?.child_key(neighbor_idx)?;
        let child_rect = self.child_rect_for_key(child)?;
        let original_span = if is_horizontal(edge) {
            child_rect.size.w
        } else if is_vertical(edge) {
            child_rect.size.h
        } else {
            0.0
        };

        Some(ResizeTarget {
            parent,
            child,
            neighbor,
            neighbor_after: neighbor_idx > child_idx,
            original_span,
        })
    }

    /// Coordinate of the boundary between the target's two children, along `edge`'s axis.
    fn resize_boundary_coord(&self, target: &ResizeTarget, edge: ResizeEdge) -> Option<f64> {
        let child_rect = self.child_rect_for_key(target.child)?;
        let neighbor_rect = self.child_rect_for_key(target.neighbor)?;

        if is_horizontal(edge) {
            let (near, far) = if neighbor_rect.loc.x < child_rect.loc.x {
                (neighbor_rect.loc.x + neighbor_rect.size.w, child_rect.loc.x)
            } else {
                (child_rect.loc.x + child_rect.size.w, neighbor_rect.loc.x)
            };
            return Some((near + far) / 2.0);
        }

        if is_vertical(edge) {
            let (near, far) = if neighbor_rect.loc.y < child_rect.loc.y {
                (neighbor_rect.loc.y + neighbor_rect.size.h, child_rect.loc.y)
            } else {
                (child_rect.loc.y + child_rect.size.h, neighbor_rect.loc.y)
            };
            return Some((near + far) / 2.0);
        }

        None
    }

    /// Span available to the target's container children after subtracting gaps, or None
    /// when the container no longer splits along `layout`'s axis.
    pub(in crate::layout) fn resize_available_span(
        &self,
        target: &ResizeTarget,
        layout: Layout,
    ) -> Option<f64> {
        let container = self.get_container(target.parent)?;
        let child_count = container.child_count();
        if container.layout() != layout || child_count == 0 {
            return None;
        }

        let rect = container.geometry();
        let total = match layout {
            Layout::SplitH => rect.size.w,
            Layout::SplitV => rect.size.h,
            Layout::Tabbed | Layout::Stacked => return None,
        };

        let gap = self.gap_in(target.parent);
        let available = (total - gap * (child_count as f64 - 1.0)).max(0.0);
        (available > 0.0).then_some(available)
    }

    /// Current size share of the target's child within its container.
    pub(in crate::layout) fn resize_current_percent(&self, target: &ResizeTarget) -> f64 {
        self.child_index(target.parent, target.child)
            .and_then(|idx| self.child_percent(target.parent, idx))
            .unwrap_or(1.0)
    }

    /// Give the target's child `percent` of its container, taking the difference from its
    /// neighbour. Returns whether anything changed.
    pub(in crate::layout) fn apply_resize(
        &mut self,
        target: &ResizeTarget,
        layout: Layout,
        percent: f64,
    ) -> bool {
        let Some(child_idx) = self.child_index(target.parent, target.child) else {
            return false;
        };
        let Some(neighbor_idx) = self.child_index(target.parent, target.neighbor) else {
            return false;
        };

        let Some(container) = self.get_container(target.parent) else {
            return false;
        };
        if container.layout() != layout {
            return false;
        }
        self.set_child_percent_pair(target.parent, child_idx, neighbor_idx, percent)
    }

    /// Which edges of `leaf_key` are being dragged, given the active resize targets. A leaf
    /// is affected when it lives under either side of a target.
    pub(in crate::layout) fn resize_edges_for_leaf(
        &self,
        leaf_key: NodeKey,
        horizontal: Option<&ResizeTarget>,
        vertical: Option<&ResizeTarget>,
    ) -> ResizeEdge {
        let mut edges = ResizeEdge::empty();

        if let Some(target) = horizontal {
            if let Some(edge) =
                self.resize_edge_for_leaf(leaf_key, target, ResizeEdge::LEFT, ResizeEdge::RIGHT)
            {
                edges |= edge;
            }
        }

        if let Some(target) = vertical {
            if let Some(edge) =
                self.resize_edge_for_leaf(leaf_key, target, ResizeEdge::TOP, ResizeEdge::BOTTOM)
            {
                edges |= edge;
            }
        }

        edges
    }

    /// The edge of `leaf_key` that faces the target's boundary, if the leaf is under one of
    /// the target's two sides.
    fn resize_edge_for_leaf(
        &self,
        leaf_key: NodeKey,
        target: &ResizeTarget,
        near_edge: ResizeEdge,
        far_edge: ResizeEdge,
    ) -> Option<ResizeEdge> {
        // Find the ancestor of the leaf that is a direct child of the target's container.
        let mut current = leaf_key;
        let branch = loop {
            let parent = self.parent_of(current)?;
            if parent == target.parent {
                break current;
            }
            current = parent;
        };

        if branch == target.child {
            Some(if target.neighbor_after {
                far_edge
            } else {
                near_edge
            })
        } else if branch == target.neighbor {
            Some(if target.neighbor_after {
                near_edge
            } else {
                far_edge
            })
        } else {
            None
        }
    }
}
